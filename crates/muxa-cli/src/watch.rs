//! `muxa watch` — fullscreen ratatui dashboard.
//!
//! Polls the daemon via `Client::snapshot()` every 500 ms and renders a
//! live-updating table of tracked agents. Input is handled via crossterm
//! events:
//!
//! - Direct filtering: printable characters immediately narrow the row set;
//!   `/` explicitly arms the filter so queries may start with a reserved key.
//!   Backspace edits and Esc clears before quitting.
//! - Navigation: arrows always move between sessions. While the filter is
//!   empty, `hjkl`, `gg` / `G`, Home / End, page keys, and Ctrl-U / Ctrl-D add
//!   conventional TUI navigation. Enter opens the prompt composer (`Enter`
//!   again on an empty prompt attaches).
//! - Inspection: `Alt-P` opens preview, `Alt-I` toggles the responsive split
//!   inspector, and `Alt-E` opens the persistent transition inbox.
//! - Destructive and sort actions retain Alt chords (`Alt-K`, `Alt-X`, …),
//!   while common browse actions also have `o`, `r`, `?`, and `:` aliases.
//! - Discovery: F1 / `Alt-?` toggles the complete binding reference.
//!
//! Terminal lifecycle is managed by a RAII `TerminalGuard` so raw mode and
//! the alternate screen are always restored — even on panic.

use std::future::Future;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::collaboration::{
    AirArtifactProfile, AirArtifactReference, CollaborationOrigin, CollaborationRequest,
    NewRequest, Participant, RequestKind, RequestMailbox, RequestStatus, RoomContext, WorkMode,
};
use muxa::config::{
    IconSet, WatchConfig, WatchSortKey, WatchSummary, WatchTheme, WatchView, WidthSpec,
};
use muxa::event::RateLimitScope;
use muxa::ipc::{Client, RuntimeError};
use muxa::process_tree::WorkloadProcessKind;
use muxa::session_activity::SessionActivity;
use muxa::state::Agent;
use muxa::tmux::{PaneInfo, SessionInfo};
use muxa::{ActivityEntry, HumanInteractionEntry, HumanInteractionInput, HumanInteractionKind};
use muxa::{AgentKind, AgentState};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState,
};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, HashSet, VecDeque};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

/// Polling cadence when no streaming `Subscribe` is active. We still
/// fall back to this if the daemon doesn't speak the streaming
/// variant or the subscription drops mid-session.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Slower fallback cadence when streaming `Subscribe` is wired. Push
/// updates land in ~milliseconds, so the polling tick only exists
/// to catch up after broadcast `Lagged` drops or transient
/// connection blips. 5 s gives plenty of headroom while keeping
/// idle CPU effectively zero.
const STREAMING_FALLBACK_INTERVAL: Duration = Duration::from_secs(5);
/// Max time to wait for a single keystroke when the input buffer is
/// empty. ~60 Hz so a press feels immediate without burning CPU on an
/// idle terminal. Held keys / fast typing are absorbed by the
/// drain-all-pending pattern in `run`, so this only governs idle
/// responsiveness.
const INPUT_POLL: Duration = Duration::from_millis(16);

/// Idle repaint cadence. The render loop normally repaints only when
/// something changed (input, a refresh outcome, a preview recapture). This
/// floor keeps time-derived cells fresh anyway — the Activity column renders
/// `relative_time` at second granularity — without the old behaviour of one
/// full render per `INPUT_POLL` tick (~62 fps) even on a completely idle UI.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(1);
/// Faster idle repaint cadence for the swarm view so its dot spinners
/// animate smoothly (~8 fps) even with no input.
const SWARM_REDRAW_INTERVAL: Duration = Duration::from_millis(120);

/// How long a transient action hint stays visible in the footer before
/// the renderer falls back to the default keybinding strip. 2 s is the
/// sweet spot from the spec: long enough to catch with a glance after
/// hitting `K`/`R`/`c`, short enough not to mask the next interaction.
const FOOTER_HINT_TTL: Duration = Duration::from_secs(2);

/// Delay between injecting prompt text into a tmux pane and sending the
/// submit key. Codex's TUI coalesces very fast input bursts as pasted
/// content; without a small gap, the trailing Enter can be swallowed into
/// that burst and the prompt stays composed until the user presses Enter
/// again manually.
const PROMPT_SUBMIT_GRACE: Duration = muxa::backend::PROMPT_SUBMIT_GRACE;

/// Channel capacity for the wake signal sent from the input loop to the
/// background refresh task. Capacity 1 is intentional: when the user mashes
/// `r`, we want extra requests to coalesce into a single pending wake rather
/// than queue up.
const WAKE_CAPACITY: usize = 1;
/// Channel capacity for refresh outcomes flowing from the background task to
/// the main loop. 2 is just enough to absorb a tick that lands while the
/// main task is mid-render without stalling the refresh task; the main loop
/// always drains all pending outcomes before each render.
const OUTCOME_CAPACITY: usize = 2;

#[derive(Debug, Clone, Copy)]
struct WatchThemeSpec {
    title: &'static str,
    accent: Color,
    accent_fg: Color,
    action: Color,
    action_fg: Color,
    key_bg: Color,
    key_fg: Color,
    border: Color,
    dim: Color,
    table_header: Color,
    selected_bg: Color,
    selected_fg: Option<Color>,
    state_idle: Color,
    state_working: Color,
    state_waiting: Color,
    state_choice: Color,
    state_error: Color,
    state_starting: Color,
    border_type: BorderType,
}

impl WatchThemeSpec {
    fn accent_badge(self) -> Style {
        Style::default()
            .fg(self.accent_fg)
            .bg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    fn action_badge(self) -> Style {
        Style::default().fg(self.action_fg).bg(self.action)
    }

    fn key_badge(self) -> Style {
        Style::default().fg(self.key_fg).bg(self.key_bg)
    }

    fn border_style(self) -> Style {
        Style::default().fg(self.border)
    }

    fn dim_style(self) -> Style {
        Style::default().fg(self.dim)
    }

    fn table_header_style(self) -> Style {
        Style::default()
            .fg(self.table_header)
            .add_modifier(Modifier::BOLD)
    }

    fn selected_style(self) -> Style {
        let style = Style::default()
            .bg(self.selected_bg)
            .add_modifier(Modifier::BOLD);
        if let Some(fg) = self.selected_fg {
            style.fg(fg)
        } else {
            style
        }
    }

    fn state_style(self, state: AgentState) -> Style {
        match state {
            AgentState::Idle => Style::default()
                .fg(self.state_idle)
                .add_modifier(Modifier::BOLD),
            AgentState::Working => Style::default()
                .fg(self.state_working)
                .add_modifier(Modifier::BOLD),
            AgentState::WaitingInput => Style::default()
                .fg(self.state_waiting)
                .add_modifier(Modifier::BOLD),
            AgentState::WaitingChoice => Style::default()
                .fg(self.state_choice)
                .add_modifier(Modifier::BOLD),
            AgentState::Error => Style::default()
                .fg(self.state_error)
                .add_modifier(Modifier::BOLD),
            AgentState::Starting => Style::default().fg(self.state_starting),
            AgentState::Stopped => Style::default().fg(self.dim).add_modifier(Modifier::DIM),
        }
    }
}

fn watch_theme(theme: WatchTheme) -> WatchThemeSpec {
    match theme {
        WatchTheme::Classic => classic_watch_theme(),
        WatchTheme::OhMyMuxa => oh_my_muxa_watch_theme(),
        WatchTheme::Focus => focus_watch_theme(),
        WatchTheme::Ops => ops_watch_theme(),
        WatchTheme::Mono => mono_watch_theme(),
        WatchTheme::HighContrast => high_contrast_watch_theme(),
        WatchTheme::Minimal => minimal_watch_theme(),
    }
}

fn classic_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " muxa watch ",
        accent: Color::Cyan,
        accent_fg: Color::Black,
        action: Color::Green,
        action_fg: Color::Black,
        key_bg: Color::Gray,
        key_fg: Color::Black,
        border: Color::DarkGray,
        dim: Color::DarkGray,
        table_header: Color::Gray,
        selected_bg: Color::DarkGray,
        selected_fg: None,
        state_idle: Color::Green,
        state_working: Color::Yellow,
        state_waiting: Color::Yellow,
        state_choice: Color::LightYellow,
        state_error: Color::Red,
        state_starting: Color::Cyan,
        border_type: BorderType::Plain,
    }
}

fn oh_my_muxa_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " oh-my-muxa ",
        accent: Color::Rgb(177, 139, 255),
        accent_fg: Color::Black,
        action: Color::Rgb(93, 230, 138),
        action_fg: Color::Black,
        key_bg: Color::Rgb(66, 74, 92),
        key_fg: Color::White,
        border: Color::Rgb(94, 234, 212),
        dim: Color::Gray,
        table_header: Color::Rgb(94, 234, 212),
        selected_bg: Color::Rgb(52, 45, 67),
        selected_fg: Some(Color::White),
        state_idle: Color::Rgb(93, 230, 138),
        state_working: Color::Rgb(255, 211, 105),
        state_waiting: Color::Rgb(255, 176, 86),
        state_choice: Color::Rgb(219, 181, 255),
        state_error: Color::Rgb(255, 91, 107),
        state_starting: Color::Rgb(94, 234, 212),
        border_type: BorderType::Rounded,
    }
}

fn focus_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " muxa focus ",
        accent: Color::Rgb(125, 211, 252),
        accent_fg: Color::Black,
        action: Color::Rgb(134, 239, 172),
        action_fg: Color::Black,
        key_bg: Color::DarkGray,
        key_fg: Color::White,
        border: Color::DarkGray,
        dim: Color::DarkGray,
        table_header: Color::Rgb(125, 211, 252),
        selected_bg: Color::Rgb(30, 58, 90),
        selected_fg: Some(Color::White),
        state_idle: Color::DarkGray,
        state_working: Color::Rgb(125, 211, 252),
        state_waiting: Color::Yellow,
        state_choice: Color::LightYellow,
        state_error: Color::Red,
        state_starting: Color::Cyan,
        border_type: BorderType::Plain,
    }
}

fn ops_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " muxa ops ",
        accent: Color::Yellow,
        accent_fg: Color::Black,
        action: Color::Green,
        action_fg: Color::Black,
        key_bg: Color::Rgb(64, 64, 64),
        key_fg: Color::White,
        border: Color::Yellow,
        dim: Color::Gray,
        table_header: Color::Yellow,
        selected_bg: Color::Rgb(80, 54, 0),
        selected_fg: Some(Color::White),
        state_idle: Color::Green,
        state_working: Color::Cyan,
        state_waiting: Color::LightYellow,
        state_choice: Color::Magenta,
        state_error: Color::LightRed,
        state_starting: Color::LightCyan,
        border_type: BorderType::Plain,
    }
}

fn mono_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " muxa mono ",
        accent: Color::White,
        accent_fg: Color::Black,
        action: Color::White,
        action_fg: Color::Black,
        key_bg: Color::DarkGray,
        key_fg: Color::White,
        border: Color::Gray,
        dim: Color::DarkGray,
        table_header: Color::White,
        selected_bg: Color::Gray,
        selected_fg: Some(Color::Black),
        state_idle: Color::Gray,
        state_working: Color::White,
        state_waiting: Color::White,
        state_choice: Color::White,
        state_error: Color::White,
        state_starting: Color::Gray,
        border_type: BorderType::Plain,
    }
}

fn high_contrast_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " muxa high-contrast ",
        accent: Color::White,
        accent_fg: Color::Black,
        action: Color::Black,
        action_fg: Color::White,
        key_bg: Color::White,
        key_fg: Color::Black,
        border: Color::White,
        dim: Color::Gray,
        table_header: Color::White,
        selected_bg: Color::White,
        selected_fg: Some(Color::Black),
        state_idle: Color::LightGreen,
        state_working: Color::LightCyan,
        state_waiting: Color::Yellow,
        state_choice: Color::LightMagenta,
        state_error: Color::LightRed,
        state_starting: Color::LightBlue,
        border_type: BorderType::Rounded,
    }
}

fn minimal_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " muxa ",
        accent: Color::White,
        accent_fg: Color::Black,
        action: Color::White,
        action_fg: Color::Black,
        key_bg: Color::DarkGray,
        key_fg: Color::White,
        border: Color::DarkGray,
        dim: Color::DarkGray,
        table_header: Color::White,
        selected_bg: Color::Gray,
        selected_fg: Some(Color::Black),
        state_idle: Color::White,
        state_working: Color::White,
        state_waiting: Color::White,
        state_choice: Color::White,
        state_error: Color::White,
        state_starting: Color::White,
        border_type: BorderType::Plain,
    }
}

/// A single column in the watch TUI. The set of valid columns is fixed by
/// this enum — the `[watch]` config picks which ones to show and in what
/// order, but cannot introduce new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchColumn {
    Pane,
    Kind,
    State,
    StateAge,
    Model,
    Ctx,
    Cost,
    Limits,
    Workload,
    Prompt,
    Activity,
    SessionTime,
}

impl WatchColumn {
    /// Parse a config-string column key. Returns `None` for unknown keys
    /// so the caller can warn and skip rather than refuse to load.
    fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "pane" => Self::Pane,
            "kind" => Self::Kind,
            "state" => Self::State,
            "state_age" => Self::StateAge,
            "model" => Self::Model,
            "ctx" => Self::Ctx,
            "cost" => Self::Cost,
            "limits" => Self::Limits,
            "workload" => Self::Workload,
            "prompt" => Self::Prompt,
            "activity" => Self::Activity,
            "session_time" => Self::SessionTime,
            _ => return None,
        })
    }

    fn header(self) -> &'static str {
        match self {
            Self::Pane => "NAME",
            Self::Kind => "KIND",
            Self::State => "ST",
            Self::StateAge => "STATE",
            Self::Model => "MODEL",
            Self::Ctx => "CTX%",
            Self::Cost => "COST$",
            Self::Limits => "LIMITS",
            Self::Workload => "TREE",
            Self::Prompt => "LAST PROMPT",
            Self::Activity => "ACT",
            Self::SessionTime => "DUR",
        }
    }

    fn default_width(self) -> Constraint {
        match self {
            // PANE — "session:window.pane" can run long; 22 covers most.
            Self::Pane => Constraint::Length(22),
            Self::Kind | Self::StateAge => Constraint::Length(12),
            // STATE — compact colored state marker; full state remains
            // available in detail placeholders and structured snapshots.
            Self::State => Constraint::Length(3),
            Self::Model => Constraint::Length(16),
            Self::Ctx => Constraint::Length(5),
            Self::Activity | Self::SessionTime => Constraint::Length(6),
            Self::Cost => Constraint::Length(7),
            // LIMITS — widest realistic payload is `⛔ 7d in 23h 59m`
            // (~17 cells with a wide-cell emoji). Wider columns crowd the
            // prompt; narrower ones clip the duration suffix on
            // emoji-greedy fonts.
            Self::Limits => Constraint::Length(18),
            Self::Workload => Constraint::Length(8),
            Self::Prompt => Constraint::Min(20),
        }
    }

    /// Build the `Text` content for one cell. Returning `Text` (rather
    /// than a finished `Cell`) lets the caller stack a second line on top
    /// of it when the row is selected and a detail template is enabled.
    fn agent_text<'a>(
        self,
        a: &'a Agent,
        now: OffsetDateTime,
        panes: &'a [PaneInfo],
        theme: WatchThemeSpec,
        spin: Spinner,
        summary: WatchSummary,
    ) -> Text<'a> {
        match self {
            Self::Pane => {
                let label = pane_display(a.pane.as_deref(), panes);
                // Dim the pane cell when there's nothing to attach to —
                // a deliberate visual signal that Enter won't do anything
                // useful for this row. Keeping the rest of the row's
                // columns at full brightness preserves readability of
                // state/prompt/etc.
                if a.pane.is_none() {
                    Text::from(Span::styled(
                        label,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM | Modifier::ITALIC),
                    ))
                } else {
                    label.into()
                }
            }
            Self::Kind => a.kind.to_string().into(),
            Self::State => {
                let (symbol, style) = state_marker(a.state, theme, spin);
                Text::from(Span::styled(symbol, style))
            }
            Self::StateAge => state_age_text(a, now, theme, spin),
            Self::Model => a.model.as_deref().unwrap_or("-").to_string().into(),
            Self::Ctx => a
                .context_used_pct
                .map_or_else(|| "-".into(), |p| format!("{p:>3.0}%"))
                .into(),
            Self::Cost => a
                .cost_usd
                .map_or_else(|| "-".into(), |c| format!("${c:.2}"))
                .into(),
            Self::Limits => limits_text(a, now),
            Self::Workload => workload_text(a),
            Self::Prompt => summary_line(a, summary).into(),
            Self::Activity => relative_time(a.last_activity_at, now).into(),
            Self::SessionTime => Text::from(Span::styled(
                "-",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )),
        }
    }

    fn bare_text(self, p: &PaneInfo) -> Text<'_> {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        match self {
            Self::Pane => {
                let pane = format!("{}:{}.{}", p.session, p.window_index, p.pane_index);
                Text::from(Span::styled(pane, dim))
            }
            // Bare panes have no agent metadata. We surface the pane title /
            // current command in the prompt slot so the row still carries
            // useful at-a-glance info; everything else collapses to a dash.
            Self::Prompt => {
                let summary = if p.title.is_empty() || p.title == p.current_command {
                    p.current_command.clone()
                } else {
                    format!("{}  {}", p.current_command, p.title)
                };
                let summary: String = summary.chars().take(80).collect();
                Text::from(Span::styled(summary, dim))
            }
            Self::Kind | Self::State | Self::StateAge => Text::from(Span::styled("—", dim)),
            Self::Model
            | Self::Ctx
            | Self::Cost
            | Self::Limits
            | Self::Workload
            | Self::Activity
            | Self::SessionTime => Text::from(Span::styled("-", dim)),
        }
    }

    fn session_text<'a>(
        self,
        s: &'a SessionRow,
        now: OffsetDateTime,
        panes: &'a [PaneInfo],
        theme: WatchThemeSpec,
        spin: Spinner,
        summary: WatchSummary,
    ) -> Text<'a> {
        let dim = theme.dim_style().add_modifier(Modifier::DIM);
        if matches!(self, Self::StateAge) {
            return session_state_age_text(s, now, theme, spin);
        }
        let Some(agent) = s.latest_agent.as_ref() else {
            return match self {
                Self::Pane => session_label(s, theme, spin),
                Self::Prompt => Text::from(Span::styled(
                    s.bare_summary.clone().unwrap_or_else(|| {
                        format!("{} pane{}", s.pane_count, plural(s.pane_count))
                    }),
                    dim,
                )),
                Self::SessionTime => session_time_text(s, now),
                Self::Kind | Self::State | Self::StateAge => Text::from(Span::styled("—", dim)),
                Self::Model | Self::Ctx | Self::Cost | Self::Limits | Self::Activity => {
                    Text::from(Span::styled("-", dim))
                }
                Self::Workload => Text::from(Span::styled("-", dim)),
            };
        };

        match self {
            Self::Pane => session_label(s, theme, spin),
            Self::Kind
            | Self::State
            | Self::StateAge
            | Self::Model
            | Self::Ctx
            | Self::Cost
            | Self::Limits
            | Self::Prompt
            | Self::Activity => self.agent_text(agent, now, panes, theme, spin, summary),
            Self::Workload => session_workload_text(s),
            Self::SessionTime => session_time_text(s, now),
        }
    }
}

/// Resolve the configured column list against `WatchConfig`. Unknown keys
/// are skipped with a `tracing::warn!` rather than aborting.
pub(crate) fn resolve_columns(cfg: &WatchConfig) -> Vec<WatchColumn> {
    let mut out = Vec::with_capacity(cfg.columns.len());
    for key in &cfg.columns {
        if let Some(c) = WatchColumn::from_key(key) {
            out.push(c);
        } else {
            tracing::warn!(column = %key, "unknown watch column key, skipping");
        }
    }
    // Warn (once each) on widths entries with no matching column. This
    // catches typos like `widths.prompts = 30` that would otherwise be
    // silently ignored.
    for key in cfg.widths.keys() {
        if WatchColumn::from_key(key).is_none() {
            tracing::warn!(column = %key, "unknown watch.widths key, ignoring");
        }
    }
    out
}

/// Resolve columns and apply the session-view duration default. This is used
/// both at startup and when `:view` changes granularity at runtime so the two
/// paths cannot drift into different table shapes.
fn resolve_display_columns(cfg: &WatchConfig) -> Vec<WatchColumn> {
    let mut columns = resolve_columns(cfg);
    if cfg.view == WatchView::Session && !columns.contains(&WatchColumn::SessionTime) {
        if columns.as_slice()
            == [
                WatchColumn::Pane,
                WatchColumn::State,
                WatchColumn::Activity,
                WatchColumn::Prompt,
            ]
        {
            columns = vec![
                WatchColumn::Pane,
                WatchColumn::SessionTime,
                WatchColumn::Activity,
                WatchColumn::Prompt,
            ];
        } else {
            let insert_at = columns
                .iter()
                .position(|c| matches!(c, WatchColumn::Prompt | WatchColumn::Activity))
                .unwrap_or(columns.len());
            columns.insert(insert_at, WatchColumn::SessionTime);
        }
    }
    columns
}

/// Resolve the width Constraint for `col`, falling back to its default
/// when the config has no entry or an `Invalid` spec.
fn resolve_width(col: WatchColumn, cfg: &WatchConfig) -> Constraint {
    let key = col.config_key();
    match cfg.widths.get(key) {
        Some(WidthSpec::Length(n)) => Constraint::Length(*n),
        Some(WidthSpec::Min(n)) => Constraint::Min(*n),
        Some(WidthSpec::Percentage(n)) => Constraint::Percentage((*n).min(100)),
        Some(WidthSpec::Invalid(raw)) => {
            tracing::warn!(column = %key, value = %raw, "invalid watch.widths value, using default");
            col.default_width()
        }
        None => col.default_width(),
    }
}

impl WatchColumn {
    fn config_key(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Kind => "kind",
            Self::State => "state",
            Self::StateAge => "state_age",
            Self::Model => "model",
            Self::Ctx => "ctx",
            Self::Cost => "cost",
            Self::Limits => "limits",
            Self::Workload => "workload",
            Self::Prompt => "prompt",
            Self::Activity => "activity",
            Self::SessionTime => "session_time",
        }
    }
}

/// One row of the dashboard. Either a tracked muxa agent or a plain tmux
/// pane the daemon doesn't know about — listing both makes `muxa watch` a
/// drop-in replacement for tmux's `choose-tree -Zs`.
pub(crate) enum WatchRow {
    Agent(Box<Agent>),
    BarePane(Box<PaneInfo>),
    Session(Box<SessionRow>),
}

/// Stable identity of a table row, used to keep the cursor pinned to the
/// same session / agent / pane across refreshes and re-sorts.
///
/// Deliberately *not* based on `pane_id`: a session row's
/// `representative_pane` is whichever of its agents was most recently
/// active, so it changes on its own as the session works. Identity has to
/// survive that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowIdentity {
    Agent(AgentKind, String),
    BarePane(String),
    Session(String),
}

/// One selectable line after runtime filtering and optional session
/// expansion have been applied. `agent_idx` points at a child agent inside a
/// session row; `None` is the ordinary top-level row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleTarget {
    row_idx: usize,
    agent_idx: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchEventKind {
    Done,
    Attention,
    Error,
}

#[derive(Debug, Clone)]
struct WatchEventEntry {
    kind: WatchEventKind,
    state: AgentState,
    label: String,
    summary: String,
    occurred_at: OffsetDateTime,
}

const MAX_WATCH_EVENTS: usize = 50;

#[derive(Debug, Clone)]
pub(crate) struct SessionRow {
    /// Raw session id — `PaneInfo.session` (tmux session name / herdr
    /// `workspace_id`). This is the ledger/display key: activity lookups and
    /// `display_name` resolution key off it (host id-spaces are disjoint, so a
    /// raw id is unambiguous for those lookups). NOT the grouping key — a tmux
    /// session and a herdr workspace can share a raw id (both "w1"), so
    /// grouping/identity use `group_key` instead.
    pub session: String,
    /// Host-namespaced grouping/identity key (`"{host}:{session}"`, e.g.
    /// `"tmux:w1"` vs `"herdr:w1"`). Keeps a tmux session and a herdr workspace
    /// with the same raw id in distinct rows. Internal only — never displayed.
    pub group_key: String,
    /// Human-facing name shown in the session view. Equals `session` on tmux;
    /// resolves to the workspace label on herdr.
    pub display_name: String,
    pub pane_ids: Vec<String>,
    pub representative_pane: Option<String>,
    pub latest_agent: Option<Agent>,
    pub agents: Vec<Agent>,
    pub pane_count: usize,
    pub bare_summary: Option<String>,
    pub activity: Option<SessionActivity>,
    agent_states: HashMap<(AgentKind, String), AgentState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchSortPreset {
    Session,
    Latest,
    Duration,
    State,
}

impl WatchSortPreset {
    fn keys(self) -> Vec<WatchSortKey> {
        match self {
            Self::Session => vec![WatchSortKey::Session, WatchSortKey::Activity],
            Self::Latest => vec![WatchSortKey::Activity],
            Self::Duration => vec![WatchSortKey::SessionTime],
            Self::State => vec![WatchSortKey::State, WatchSortKey::Activity],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Session => "SESSION",
            Self::Latest => "LATEST",
            Self::Duration => "DUR",
            Self::State => "ST",
        }
    }
}

fn sort_label(keys: &[WatchSortKey]) -> &'static str {
    match keys.first().copied() {
        Some(WatchSortKey::Session) | None => "SESSION",
        Some(WatchSortKey::Activity) => "LATEST",
        Some(WatchSortKey::SessionTime) => "DUR",
        Some(WatchSortKey::State) => "ST",
        Some(WatchSortKey::Pane) => "PANE",
        Some(WatchSortKey::PaneId) => "PANE ID",
    }
}

fn sort_key_toml_name(key: WatchSortKey) -> &'static str {
    match key {
        WatchSortKey::Session => "session",
        WatchSortKey::Activity => "latest",
        WatchSortKey::SessionTime => "session_time",
        WatchSortKey::State => "state",
        WatchSortKey::Pane => "pane",
        WatchSortKey::PaneId => "pane_id",
    }
}

fn persist_watch_sort(path: &Path, keys: &[WatchSortKey]) -> std::result::Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };

    let mut doc = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse {}: {e}", path.display()))?
    };

    match doc.get("watch") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => return Err("[watch] is not a table".to_string()),
    }
    if doc.get("watch").is_none() {
        doc["watch"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    let watch = doc["watch"]
        .as_table_mut()
        .ok_or_else(|| "[watch] is not a table".to_string())?;
    let mut sort = toml_edit::Array::new();
    for key in keys {
        sort.push(sort_key_toml_name(*key));
    }
    watch["sort"] = toml_edit::Item::Value(toml_edit::Value::Array(sort));

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| format!("write {}: {e}", path.display()))
}

impl WatchRow {
    fn agent(agent: Agent) -> Self {
        Self::Agent(Box::new(agent))
    }

    fn pane_id(&self) -> Option<&str> {
        match self {
            Self::Agent(a) => a.pane.as_deref(),
            Self::BarePane(p) => Some(&p.pane_id),
            Self::Session(s) => s.representative_pane.as_deref(),
        }
    }

    /// Identity that survives a rebuild of the row set — see [`RowIdentity`].
    fn identity(&self) -> RowIdentity {
        match self {
            Self::Agent(a) => RowIdentity::Agent(a.kind, a.session_id.clone()),
            Self::BarePane(p) => RowIdentity::BarePane(p.pane_id.clone()),
            Self::Session(s) => RowIdentity::Session(s.group_key.clone()),
        }
    }

    fn contains_pane(&self, pane_id: &str) -> bool {
        match self {
            Self::Agent(a) => a.pane.as_deref() == Some(pane_id),
            Self::BarePane(p) => p.pane_id == pane_id,
            Self::Session(s) => {
                s.representative_pane.as_deref() == Some(pane_id)
                    || s.pane_ids.iter().any(|id| id == pane_id)
                    || s.agents.iter().any(|a| a.pane.as_deref() == Some(pane_id))
            }
        }
    }
}

/// A daemon error to surface in the header. We track whether the inner
/// error is the "daemon not reachable" variant so the renderer can drop the
/// `daemon error: ` prefix — otherwise the message reads
/// "daemon error: daemon not reachable at …" which is awkward.
pub(crate) struct DaemonError {
    pub message: String,
    pub self_describing: bool,
}

/// A power-user action requested against the currently-selected row.
///
/// The input handler returns one of these (or `None`) and a separate
/// helper executes it via [`dispatch_quick_action`]. Keeping the
/// "what should happen" decision separate from "shell out to tmux /
/// xclip" lets unit tests verify the selection logic without touching
/// the real subprocess world — see the `Effects` trait below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuickAction {
    /// Kill the tmux pane the agent is running in. String is the
    /// `pane_id` (e.g. `%42`) we'll pass to `tmux kill-pane -t`.
    KillPane(String),
    /// Send Ctrl-C to the pane to abort the current turn. The spec
    /// originally called this "restart"; the pragmatic shape is just
    /// "abort" because we don't know the original launch command to
    /// re-run reliably across Claude/Codex/Gemini wrappers.
    AbortTurn(String),
    /// Copy the selected agent's `last_prompt` to the system clipboard.
    /// String is the prompt body; the dispatcher tries pbcopy / wl-copy
    /// / xclip in order and falls back to a temp file if none work.
    CopyPrompt(String),
    /// Send a freshly-authored prompt straight into the selected pane.
    /// The dispatcher writes the text literally, waits briefly, then
    /// sends Enter as a separate key so the target agent submits it.
    SendPrompt { pane_id: String, text: String },
    /// Toggle the `?` help overlay. Pure UI — no side-effects.
    ShowHelp,
}

/// Outcome of running a [`QuickAction`] — surfaced to the run loop
/// which then turns it into a transient footer hint or a state mutation.
///
/// Note that "not applicable" is handled at the **input-handler** stage
/// (via `Action::NotApplicable`) rather than here: by the time the
/// dispatcher runs we've already decided the action is going through,
/// so anything the dispatcher reports back is either success or a
/// real backend failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionOutcome {
    /// Action ran successfully. Message goes into the footer hint.
    Ok(String),
    /// Action failed (tmux exited non-zero, clipboard binaries all
    /// missing, …). Message goes into the footer hint with an error tone.
    Err(String),
    /// Help overlay toggled — no hint to surface; the renderer reads
    /// `app.help_open` directly.
    HelpToggled,
}

/// Side-effect surface for [`dispatch_quick_action`]. Lifting the three
/// shell-outs into a trait means tests can pass a `RecorderEffects`
/// stub that records calls without ever spawning a subprocess.
pub(crate) trait Effects {
    /// Run `tmux kill-pane -t <pane_id>`. Return Ok on exit 0.
    fn kill_pane(&mut self, pane_id: &str) -> std::result::Result<(), String>;
    /// Run `tmux send-keys -t <pane_id> C-c`. Return Ok on exit 0.
    fn send_ctrl_c(&mut self, pane_id: &str) -> std::result::Result<(), String>;
    /// Pipe `text` into the system clipboard. Returns the name of the
    /// helper that succeeded (`pbcopy` / `wl-copy` / `xclip`) or
    /// `tmpfile:<path>` if all helpers were missing and we wrote a
    /// fallback file. `Err()` when even the fallback failed.
    fn copy_to_clipboard(&mut self, text: &str) -> std::result::Result<String, String>;
    /// Send `text` to `pane_id` as literal terminal input, then press
    /// Enter after a short grace delay. Return Ok only if both tmux
    /// calls succeed.
    fn send_prompt(&mut self, pane_id: &str, text: &str) -> std::result::Result<(), String>;
}

/// Real-world `Effects` impl — shells out to tmux and the system
/// clipboard helpers. Unit tests use a recorder stub instead.
pub(crate) struct RealEffects;

impl Effects for RealEffects {
    fn kill_pane(&mut self, pane_id: &str) -> std::result::Result<(), String> {
        run_status("tmux", &["kill-pane", "-t", pane_id])
    }

    fn send_ctrl_c(&mut self, pane_id: &str) -> std::result::Result<(), String> {
        run_status("tmux", &["send-keys", "-t", pane_id, "C-c"])
    }

    fn send_prompt(&mut self, pane_id: &str, text: &str) -> std::result::Result<(), String> {
        send_prompt_to_tmux(
            pane_id,
            text,
            PROMPT_SUBMIT_GRACE,
            |args| run_status("tmux", args),
            std::thread::sleep,
        )
    }

    fn copy_to_clipboard(&mut self, text: &str) -> std::result::Result<String, String> {
        // Backend priority — first one that's *applicable* and
        // succeeds wins. We cascade past Failed (not just
        // NotFound) so an installed-but-broken backend (e.g.
        // `xclip` present without an X server) doesn't strand the
        // user; the /tmp fallback at the bottom is the safety
        // net.
        //
        //   1. tmux load-buffer when $TMUX is set — works in pure
        //      SSH/headless because tmux owns its own paste buffer
        //      (`prefix + ]`) and on tmux 3.2+ with
        //      `set -g set-clipboard on` it transparently forwards
        //      to the host terminal's clipboard via OSC 52. Single
        //      backend that covers most of the "remote dev" case.
        //   2. pbcopy on macOS (unconditional — pbcopy is always
        //      present on Apple shells).
        //   3. wl-copy when $WAYLAND_DISPLAY is set.
        //   4. xclip / xsel when $DISPLAY is set. Pre-flight env
        //      check skips them entirely on headless hosts so we
        //      don't even produce the misleading "exit 1" the user
        //      hit before this fix.
        //   5. /tmp/muxa-clip-<ts>.txt as the last resort.
        struct Cand<'a> {
            label: &'a str,
            bin: &'a str,
            args: &'a [&'a str],
            applicable: bool,
        }

        let in_tmux = std::env::var_os("TMUX").is_some();
        let in_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let in_x11 = std::env::var_os("DISPLAY").is_some();
        let candidates: &[Cand] = &[
            Cand {
                label: "tmux",
                bin: "tmux",
                args: &["load-buffer", "-"],
                applicable: in_tmux,
            },
            Cand {
                label: "pbcopy",
                bin: "pbcopy",
                args: &[],
                applicable: cfg!(target_os = "macos"),
            },
            Cand {
                label: "wl-copy",
                bin: "wl-copy",
                args: &[],
                applicable: in_wayland,
            },
            Cand {
                label: "xclip",
                bin: "xclip",
                args: &["-selection", "clipboard"],
                applicable: in_x11,
            },
            Cand {
                label: "xsel",
                bin: "xsel",
                args: &["--clipboard", "--input"],
                applicable: in_x11,
            },
        ];
        for c in candidates {
            if !c.applicable {
                continue;
            }
            // Cascade past Failed AND NotFound — the user never saw
            // a successful copy, so trying the next backend is
            // strictly more useful than surfacing one backend's
            // error.
            if pipe_to_command(c.bin, c.args, text).is_ok() {
                return Ok(c.label.to_string());
            }
        }
        // Last-resort fallback: dump to /tmp so the user can `cat` it.
        let path = format!(
            "/tmp/muxa-clip-{}.txt",
            OffsetDateTime::now_utc().unix_timestamp()
        );
        std::fs::write(&path, text).map_err(|e| format!("write {path}: {e}"))?;
        Ok(format!("tmpfile:{path}"))
    }
}

fn send_prompt_to_tmux<R, S>(
    pane_id: &str,
    text: &str,
    submit_delay: Duration,
    mut run_tmux: R,
    mut sleep: S,
) -> std::result::Result<(), String>
where
    R: FnMut(&[&str]) -> std::result::Result<(), String>,
    S: FnMut(Duration),
{
    run_tmux(&["send-keys", "-t", pane_id, "-l", "--", text])?;
    if !submit_delay.is_zero() {
        sleep(submit_delay);
    }
    run_tmux(&["send-keys", "-t", pane_id, "Enter"])
}

/// Result of attempting to spawn-and-pipe to a clipboard helper.
enum PipeErr {
    /// The binary itself wasn't on PATH — caller should try the next.
    NotFound,
    /// The binary ran but returned non-zero, or stdin write failed.
    /// The String describes the failure for ad-hoc logging; current
    /// callers cascade past `Failed` to the next backend so the
    /// payload isn't surfaced anywhere user-visible — but it stays
    /// for debug traces and future use.
    #[allow(dead_code)]
    Failed(String),
}

fn pipe_to_command(bin: &str, args: &[&str], text: &str) -> std::result::Result<(), PipeErr> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(PipeErr::NotFound),
        Err(e) => return Err(PipeErr::Failed(format!("{bin}: {e}"))),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            return Err(PipeErr::Failed(format!("{bin} stdin: {e}")));
        }
    }
    match child.wait() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(PipeErr::Failed(format!(
            "{bin} exited with {}",
            s.code().map_or_else(|| "signal".into(), |c| c.to_string())
        ))),
        Err(e) => Err(PipeErr::Failed(format!("{bin}: {e}"))),
    }
}

fn run_status(bin: &str, args: &[&str]) -> std::result::Result<(), String> {
    match Command::new(bin).args(args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "{bin} {} exited with {}",
            args.join(" "),
            s.code().map_or_else(|| "signal".into(), |c| c.to_string())
        )),
        Err(e) => Err(format!("{bin}: {e}")),
    }
}

/// Run a [`QuickAction`] against an [`Effects`] sink and return what
/// the run loop should surface in the footer. Pure with respect to UI
/// state — the caller mutates `App` based on the returned outcome.
pub(crate) fn dispatch_quick_action(action: QuickAction, fx: &mut dyn Effects) -> ActionOutcome {
    match action {
        QuickAction::KillPane(pane_id) => match fx.kill_pane(&pane_id) {
            Ok(()) => ActionOutcome::Ok(format!("✔ killed pane {pane_id}")),
            Err(e) => ActionOutcome::Err(format!("✗ kill-pane failed: {e}")),
        },
        QuickAction::AbortTurn(pane_id) => match fx.send_ctrl_c(&pane_id) {
            Ok(()) => ActionOutcome::Ok(format!("✔ sent Ctrl-C to {pane_id}")),
            Err(e) => ActionOutcome::Err(format!("✗ abort failed: {e}")),
        },
        QuickAction::CopyPrompt(text) => match fx.copy_to_clipboard(&text) {
            Ok(via) if via.starts_with("tmpfile:") => {
                let path = via.trim_start_matches("tmpfile:");
                ActionOutcome::Ok(format!(
                    "✔ wrote prompt to {path} (no clipboard tool found)"
                ))
            }
            Ok(via) => ActionOutcome::Ok(format!("✔ copied prompt via {via}")),
            Err(e) => ActionOutcome::Err(format!("✗ copy failed: {e}")),
        },
        QuickAction::SendPrompt { pane_id, text } => match fx.send_prompt(&pane_id, &text) {
            Ok(()) => ActionOutcome::Ok(format!("✔ sent prompt to {pane_id}")),
            Err(e) => ActionOutcome::Err(format!("✗ send failed: {e}")),
        },
        QuickAction::ShowHelp => ActionOutcome::HelpToggled,
    }
}

/// Push an [`ActionOutcome`] back into `App` state — sets the footer
/// hint with the appropriate severity level, or toggles `help_open`
/// for `HelpToggled`. Centralised so the run loop and tests can both
/// drive it.
pub(crate) fn apply_outcome_to_app(app: &mut App, outcome: ActionOutcome) {
    match outcome {
        ActionOutcome::Ok(msg) => app.set_hint(msg, HintLevel::Ok),
        ActionOutcome::Err(msg) => app.set_hint(msg, HintLevel::Err),
        ActionOutcome::HelpToggled => {
            app.help_open = !app.help_open;
        }
    }
}

/// The lines of text rendered into the `?` help overlay. Returned as
/// a Vec so the snapshot test can assert on the exact contents
/// without going through ratatui. Keep this in sync with the actual
/// keybinding matrix in `handle_event` — the overlay is the user's
/// canonical reference.
pub(crate) fn help_overlay_text() -> Vec<&'static str> {
    vec![
        "Filter & navigation",
        "  type or /       filter; / allows reserved first characters",
        "  Backspace/C-W   edit filter / delete previous word",
        "  Ctrl-U / Esc    clear filter; Esc again backs out / quits",
        "  ↑/↓ · j/k       move sessions/children while browsing",
        "  ←/→ · h/l       return to parent / enter first child agent",
        "  gg/G · Home/End first / last selectable row",
        "  PgUp/PgDn       page; Ctrl-U/Ctrl-D half page",
        "  Enter          compose prompt for selected pane",
        "  empty Enter    attach to selected pane",
        "",
        "Commands & inspection",
        "  :              command palette (Tab completes)",
        "  o / Alt-P      open preview overlay",
        "  Alt-I / Alt-E  inspector / persistent event inbox",
        "  Alt-A          attention-only filter",
        "  [/] · f/c      (in preview) agent / geometry / content",
        "  Enter          (in preview) compose prompt",
        "  m / b          message selected room peer / mailbox",
        "  i / e          (in mailbox) claim inbox / reply",
        "",
        "Sorting",
        "  Alt-S/L/D/T    session / latest / duration / state",
        "State markers",
        match crate::icon_set() {
            IconSet::Unicode => {
                "  ● working  ▶ input  ◆ choice  ■ error  ○ idle  ◌ starting  × stopped"
            }
            IconSet::Ascii => {
                "  * working  > input  ? choice  ! error  o idle  ~ starting  x stopped"
            }
        },
        "  TREE: ◇ subagent  ▸ shell  + process; helper-only trees hidden",
        "",
        "Quick actions (act on selected row)",
        "  Alt-C          copy last prompt to clipboard",
        "  Alt-K          kill the pane (confirm popup)",
        "  Alt-X          abort current turn (confirm popup)",
        "  r/Ctrl-R/Alt-R force refresh while browsing",
        "  ?/F1/Alt-?     toggle help; q/Ctrl-C quits",
    ]
}

/// Modal confirmation popup state — mirrors the preview-popup pattern
/// (centred ratatui rect over the table) but with a binary y/N choice
/// instead of scrollable content.
///
/// **Why default focus is "No"**: the actions this gates (`K` kills the
/// pane, `R` aborts the agent's turn) are destructive enough that we
/// want the user to deliberately type `y` rather than fat-finger Enter.
/// Anything other than `y` / `Y` cancels — including Esc, `n`, `q`,
/// arrow keys, Tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmPopup {
    /// One-line message, e.g. `Kill pane main:2.0?`. Caller pre-formats
    /// the pane label so the popup itself stays presentation-only.
    pub message: String,
    /// What to dispatch when the user confirms with `y` / `Y`. Held
    /// here so the input handler can complete the round-trip without
    /// re-resolving the selected row (which might have moved between
    /// the popup opening and the user's reply).
    pub on_confirm: QuickAction,
}

/// Inline prompt composer opened from the table with Enter. It pins the
/// target pane at open time so background refreshes or resorting cannot
/// redirect a typed prompt to a different row.
#[derive(Debug, Clone, Default)]
struct WatchCollaboration {
    origin: Option<CollaborationOrigin>,
    room: Option<RoomContext>,
    incoming: Vec<CollaborationRequest>,
    sent: Vec<CollaborationRequest>,
    unavailable: Option<String>,
}

impl WatchCollaboration {
    fn peer_for_pane(&self, pane: &str) -> Option<&Participant> {
        self.room
            .as_ref()?
            .peers
            .iter()
            .find(|participant| participant.pane == pane)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CollaborationMailboxTab {
    #[default]
    Incoming,
    Sent,
}

#[derive(Debug, Clone, Default)]
struct CollaborationMailboxState {
    open: bool,
    tab: CollaborationMailboxTab,
    selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CollaborationComposeTarget {
    Send {
        origin: CollaborationOrigin,
        /// Raw pane id of the recipient. `target` is the wire form
        /// (`pane:%N`); this copy exists so `just send` mode can type
        /// into the pane without re-parsing its own address.
        pane: String,
        target: String,
        kind: RequestKind,
        mode: ComposeSendMode,
    },
    Reply {
        origin: CollaborationOrigin,
        request_id: String,
        status: RequestStatus,
    },
    /// Keystrokes-only composer for a pane that cannot receive a request —
    /// collaboration disabled, no room, no peer, or a row outside this
    /// window. `m` still owes the user a way to type at the agent they are
    /// pointing at; what it cannot do is dress those keystrokes up as a
    /// contract, so `Tab`/`Ctrl-E` explain instead of cycling.
    Prompt { pane: String },
}

/// What Ctrl-E cycles: how the composed text leaves the composer.
///
/// The first two are the wire `WorkMode` contract on a durable request.
/// `JustSend` is watch-local — plain keystrokes typed into the pane, no
/// request, no reply, no contract. It lives here and not in `WorkMode`
/// because that enum is wire format (PROTOCOL.md, the MCP schema,
/// `collaboration.json`), and a variant that by construction never
/// appears on the wire would burden every consumer with a case that
/// cannot happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeSendMode {
    ReadOnly,
    Execute,
    JustSend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollaborationComposer {
    target: CollaborationComposeTarget,
    label: String,
    input: String,
    cursor: usize,
}

impl CollaborationComposer {
    fn new(target: CollaborationComposeTarget, label: String) -> Self {
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

/// Editable `:` command line, with the same UTF-8-safe cursor behavior
/// as the request composer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommandPalette {
    pub input: String,
    pub cursor: usize,
}

impl CommandPalette {
    fn insert(&mut self, c: char) {
        let idx = char_to_byte_idx(&self.input, self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            self.insert(c);
        }
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

    fn delete_word(&mut self) {
        while self.cursor > 0
            && self
                .input
                .chars()
                .nth(self.cursor - 1)
                .is_some_and(char::is_whitespace)
        {
            self.backspace();
        }
        while self.cursor > 0
            && self
                .input
                .chars()
                .nth(self.cursor - 1)
                .is_some_and(|c| !c.is_whitespace())
        {
            self.backspace();
        }
    }

    fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
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

fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    s.char_indices()
        .nth(char_idx)
        .map_or_else(|| s.len(), |(idx, _)| idx)
}

/// A transient hint pinned to the footer for ~2 s after a quick action
/// runs (or fails). Replaces the default keybinding strip while active.
#[derive(Debug, Clone)]
pub(crate) struct FooterHint {
    pub message: String,
    pub level: HintLevel,
    pub set_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintLevel {
    /// Green — successful action, "✔ copied prompt".
    Ok,
    /// Red — action failed; user probably wants to try again.
    Err,
    /// Yellow — action vetoed because the row didn't qualify
    /// (paneless agent, missing prompt). Not the user's fault, but
    /// not a backend failure either.
    Warn,
}

impl FooterHint {
    fn fresh(&self) -> bool {
        self.set_at.elapsed() < FOOTER_HINT_TTL
    }
}

/// State held by the TUI.
///
/// Kept separate from rendering so the smoke test can construct it
/// directly without touching a real terminal.
#[allow(clippy::struct_excessive_bools)] // independent overlay/filter/runtime flags are clearer than a coupled state enum
pub(crate) struct App {
    pub rows: Vec<WatchRow>,
    pub table_state: TableState,
    /// Monotonic frame counter advanced once per paint. Drives the swarm
    /// view's dot-spinner animation; unused by the table views.
    pub anim_frame: usize,
    /// Last-seen state per agent, diffed on each update to detect transitions
    /// that should flash a [`Pulse`]. Keyed by `(kind, session_id)` — the
    /// codebase's compound agent identity — so two agents that happen to share
    /// a `session_id` string across kinds don't clobber each other.
    prev_agent_states: HashMap<(AgentKind, String), AgentState>,
    /// Active transition pulses, same `(kind, session_id)` key. Pruned as they
    /// expire past [`PULSE_WINDOW`].
    pulses: HashMap<(AgentKind, String), Pulse>,
    /// Incremental filter entered directly from table mode.
    pub search_query: String,
    /// True after `/` explicitly arms filter input. Unlike implicit direct
    /// typing, this remains true when the query is empty so reserved browse
    /// keys (`q`, `g`, `h`, …) can be used as the first search character.
    pub explicit_search: bool,
    /// First half of the conventional `gg` jump. Any non-`g` key clears it.
    pending_g: bool,
    /// Number of selectable rows in the most recently rendered table body.
    /// Page movement uses this rather than a hard-coded terminal size.
    table_page_rows: usize,
    /// When true, only error / input / choice targets remain visible.
    pub attention_only: bool,
    /// Session group currently expanded into individually-selectable child
    /// rows. Selection keeps this to at most one group: the selected session,
    /// or the parent session of the selected child agent.
    expanded_sessions: HashSet<String>,
    /// Wide terminals show a persistent live inspector unless explicitly
    /// disabled with Alt-I.
    pub inspector_enabled: bool,
    /// Set by the last render pass; lets the capture loop avoid polling a
    /// live pane when the terminal is too narrow to show the inspector.
    pub inspector_visible: bool,
    /// Persistent transition inbox for this watch process.
    events: VecDeque<WatchEventEntry>,
    pub unread_events: usize,
    pub event_inbox_open: bool,
    pub last_error: Option<DaemonError>,
    pub last_refresh: OffsetDateTime,
    /// Watch config — held by value so the rendering path doesn't need to
    /// re-resolve columns every frame, and the smoke tests can swap it in.
    pub watch_cfg: WatchConfig,
    /// Column set resolved from `watch_cfg` once at construction. Unknown
    /// keys are warned-and-skipped here (see `resolve_columns`).
    pub columns: Vec<WatchColumn>,
    /// Snapshot of the full tmux pane inventory from the last refresh.
    /// Used by `pane_display` to render `session:window.pane` labels for
    /// agent rows without shelling out to tmux on every render frame —
    /// per-row resolves used to cost ~5 ms each, so a 35-agent table
    /// blocked the input loop for ~175 ms per paint.
    pub panes: Vec<PaneInfo>,
    /// Snapshot of tmux sessions from the last refresh. Only populated for
    /// tmux hosts; zellij and host-down cases leave it empty.
    pub sessions: Vec<SessionInfo>,
    /// Persisted cumulative attached-time counters keyed by tmux session id.
    pub session_activity: Vec<SessionActivity>,
    /// True between a user-triggered refresh request (`r`) and the
    /// matching outcome landing on the channel. Surfaces a brief
    /// "↻ refreshing…" hint in the header so mashing `r` during a
    /// slow daemon snapshot isn't silently swallowed. Periodic
    /// background ticks intentionally don't toggle this — only the
    /// user's deliberate action lights it up.
    pub refresh_pending: bool,
    /// Pane to focus on the first non-empty data load — `$TMUX_PANE`
    /// when invoked from inside tmux. Consumed by `clamp_selection`
    /// the first time it sees rows, so subsequent refreshes don't
    /// keep snapping the cursor away from the user's manual selection.
    initial_pane: Option<String>,
    /// `Some` when the user has popped open the full-screen detail
    /// preview (`o` / `Alt-P`). The table is hidden behind the preview while
    /// this is set; `q`/`Esc`/`p` clears it.
    pub preview: Option<PreviewState>,
    /// Count of paneless agents that were filtered out of `rows` because
    /// `watch_cfg.hide_paneless` is true. Surfaced as a footer hint so
    /// users know the rows aren't lost — they just aren't actionable from
    /// the picker. Always 0 when `hide_paneless = false`.
    pub paneless_hidden: usize,
    /// Of the `paneless_hidden` agents, how many are blocked on a human
    /// (`WaitingInput` / `WaitingChoice` / `Error`). These would otherwise
    /// be completely invisible — no row, and the plain `+N paneless` footer
    /// hint doesn't say any of them need you — so the header attention
    /// summary folds this count in. Always 0 when `hide_paneless = false`
    /// (they're shown as normal rows and counted there).
    pub paneless_attention: usize,
    /// Most recent `tmux capture-pane -ep` result, keyed by `pane_id`.
    /// Populated on demand when the preview is in
    /// [`PreviewContent::LivePane`] and re-captured on every refresh
    /// tick while the preview stays open in that mode. `None` when the
    /// preview is closed or showing prompt/response content.
    pub pane_capture: Option<CapturedPane>,
    /// `Some` when a destructive action (`Alt-K`, `Alt-X`, or a `:` command)
    /// is waiting on a
    /// y/N reply. Suppresses table input and renders a centred popup;
    /// the input handler resolves the popup before any other key is
    /// interpreted.
    pub confirm: Option<ConfirmPopup>,
    collaboration: WatchCollaboration,
    collaboration_mailbox: CollaborationMailboxState,
    collaboration_composer: Option<CollaborationComposer>,
    /// Editable `:` command palette. Like other overlays it owns keyboard
    /// input until Enter executes or Esc cancels.
    pub command_palette: Option<CommandPalette>,
    /// True while the `?` help overlay is visible. Renders as a centred
    /// popup listing every keybinding — the same pattern as `confirm`,
    /// just with a static body and no follow-up dispatch.
    pub help_open: bool,
    /// Transient footer hint set after a quick action runs. The
    /// renderer hides the default keybinding strip while this is fresh
    /// (`set_at.elapsed() < FOOTER_HINT_TTL`). The run loop never
    /// explicitly clears the slot — the renderer just stops reading it.
    pub footer_hint: Option<FooterHint>,
}

/// A `muxa watch` preview overlay — detail view of the selected agent.
/// Geometry (popup vs fullscreen) and content (prompt/response vs live
/// pane capture) are independent axes so the two toggles compose: a
/// user can read the prompt in a popup, then `c` to flip to a live pane
/// view in the same popup, then `f` to fullscreen that. The selection
/// is pinned to a `pane_id` (not a row index) so background refreshes
/// that re-sort the table can't drift the preview onto a different
/// agent.
#[derive(Debug, Clone)]
pub struct PreviewState {
    /// Pane id at the time the preview was opened — also the lookup key
    /// every render frame uses to find the live agent record.
    pub pane_id: String,
    /// Vertical scroll offset in *lines from the top of the content*.
    /// `↑/↓` (or `j/k`) increment / decrement; `saturating_*` so we
    /// can't underflow past the top.
    pub scroll: u16,
    /// Whether to render as a centred popup (default — keeps the
    /// surrounding table visible for context) or as a full-screen take-
    /// over. The `f` key toggles between the two.
    pub mode: PreviewMode,
    /// What's being shown inside the box: prompt + response (default,
    /// text-only) or a live capture of the actual tmux pane contents.
    /// `c` toggles between the two.
    pub content: PreviewContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewMode {
    /// Centred popup roughly 80% × 70% of the table area. Default — the
    /// surrounding rows stay visible so the user keeps a sense of "where
    /// am I in the list" while reading the prompt/response.
    Popup,
    /// Full-screen takeover, useful for very long responses where the
    /// 80×70 box still wraps too aggressively.
    Fullscreen,
}

/// Content axis of the preview overlay. Re-exported from
/// [`muxa::config::PreviewContent`] so tests and downstream code in the
/// watch crate can keep `PreviewContent::LivePane` working without
/// having to know the enum lives in the muxa core. Independent of
/// [`PreviewMode`]'s geometry — both compose freely. The runtime
/// rendering path always reads from this enum; `[watch.preview]
/// default_content` only seeds the initial value when a fresh
/// `PreviewState` is constructed.
pub use muxa::config::PreviewContent;

/// Cached pane-capture result. One slot per `App` since only the
/// currently-previewed pane needs a capture; flipping rows or closing
/// the preview invalidates by `pane_id` mismatch and the next refresh
/// repopulates.
#[derive(Debug, Clone)]
pub struct CapturedPane {
    pub pane_id: String,
    /// Raw stdout from `tmux capture-pane -ep`, ANSI escapes intact.
    /// Decoded to ratatui `Text` lazily at render time so a stale row
    /// re-render never re-parses the same bytes.
    pub text: String,
    /// Monotonic timestamp; the main loop uses `elapsed()` to gate the
    /// next re-capture so we don't shell out every frame.
    pub fetched_at: std::time::Instant,
}

impl App {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        // Pin pane view for the pane-level tests built around this helper
        // (sorting, grouping, per-row actions, single-agent updates). The
        // *production* default is `session` (see `WatchConfig::default`);
        // session-view behavior is covered separately by config-crate tests
        // and the explicit `view: Session` cases below. Pinning here keeps
        // those pane-mechanics assertions stable regardless of the default.
        Self::with_config(WatchConfig {
            view: WatchView::Pane,
            ..WatchConfig::default()
        })
    }

    pub(crate) fn with_config(cfg: WatchConfig) -> Self {
        let columns = resolve_display_columns(&cfg);
        Self {
            rows: Vec::new(),
            table_state: TableState::default(),
            anim_frame: 0,
            prev_agent_states: HashMap::new(),
            pulses: HashMap::new(),
            search_query: String::new(),
            explicit_search: false,
            pending_g: false,
            table_page_rows: 10,
            attention_only: false,
            expanded_sessions: HashSet::new(),
            inspector_enabled: true,
            inspector_visible: false,
            events: VecDeque::new(),
            unread_events: 0,
            event_inbox_open: false,
            last_error: None,
            last_refresh: OffsetDateTime::now_utc(),
            watch_cfg: cfg,
            columns,
            panes: Vec::new(),
            sessions: Vec::new(),
            session_activity: Vec::new(),
            refresh_pending: false,
            initial_pane: None,
            preview: None,
            paneless_hidden: 0,
            paneless_attention: 0,
            pane_capture: None,
            confirm: None,
            collaboration: WatchCollaboration::default(),
            collaboration_mailbox: CollaborationMailboxState::default(),
            collaboration_composer: None,
            command_palette: None,
            help_open: false,
            footer_hint: None,
        }
    }

    /// Hint which pane should be highlighted on first load — typically
    /// `$TMUX_PANE` so launching `muxa watch` from inside tmux lands the
    /// cursor on the user's current pane instead of always row 0.
    pub(crate) fn set_initial_pane(&mut self, pane: Option<String>) {
        self.initial_pane = pane;
    }

    /// Replace the row set. `agents` (tracked) are listed first in stable
    /// order; `panes` minus any pane already represented by an agent are
    /// appended as `BarePane` rows.
    #[cfg(test)]
    pub(crate) fn set_data(&mut self, agents: Vec<Agent>, panes: Vec<PaneInfo>) {
        self.set_data_with_sessions(agents, panes, Vec::new(), Vec::new());
    }

    /// Clone the lightweight agent records needed for transition detection.
    /// A session row also keeps a cloned agent list, so this is independent of
    /// the current table granularity.
    fn current_agents(&self) -> Vec<Agent> {
        self.rows
            .iter()
            .flat_map(|r| match r {
                WatchRow::Agent(a) => vec![(**a).clone()],
                WatchRow::Session(s) => s.agents.clone(),
                WatchRow::BarePane(_) => Vec::new(),
            })
            .collect()
    }

    /// Diff the current agent states against the previous snapshot and record a
    /// [`Pulse`] for any transition worth flashing (done / error), and append
    /// durable entries to the in-process event inbox. Disabling animation
    /// suppresses pulses only; inbox tracking remains active.
    fn detect_pulses(&mut self) {
        if !self.watch_cfg.spinner {
            self.pulses.clear();
        }
        let now = std::time::Instant::now();
        let current = self.current_agents();
        for agent in &current {
            let key = (agent.kind, agent.session_id.clone());
            let previous = self.prev_agent_states.get(&key).copied();
            if self.watch_cfg.spinner {
                if let Some(pk) = pulse_kind(previous, agent.state) {
                    self.pulses.insert(
                        key.clone(),
                        Pulse {
                            kind: pk,
                            started: now,
                        },
                    );
                }
            }

            let event_kind = match (previous, agent.state) {
                (Some(AgentState::Working), AgentState::Idle) => Some(WatchEventKind::Done),
                (Some(prev), AgentState::Error) if prev != AgentState::Error => {
                    Some(WatchEventKind::Error)
                }
                (Some(prev), AgentState::WaitingInput | AgentState::WaitingChoice)
                    if prev != agent.state =>
                {
                    Some(WatchEventKind::Attention)
                }
                _ => None,
            };
            if let Some(kind) = event_kind {
                let label = agent.pane.as_deref().map_or_else(
                    || agent.session_id.clone(),
                    |pane| pane_display(Some(pane), &self.panes),
                );
                let summary = agent
                    .last_notification
                    .as_deref()
                    .or(agent.last_response.as_deref())
                    .or(agent.last_prompt.as_deref())
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                self.events.push_front(WatchEventEntry {
                    kind,
                    state: agent.state,
                    label,
                    summary,
                    occurred_at: agent.state_entered_at,
                });
                self.events.truncate(MAX_WATCH_EVENTS);
                if !self.event_inbox_open {
                    self.unread_events = self.unread_events.saturating_add(1);
                }
            }
        }
        self.prev_agent_states = current
            .into_iter()
            .map(|a| ((a.kind, a.session_id), a.state))
            .collect();
        self.pulses
            .retain(|_, p| now.duration_since(p.started) < PULSE_WINDOW);
    }

    /// The active pulse for `(kind, session_id)`, if one is still within its
    /// window.
    fn active_pulse(
        &self,
        kind: AgentKind,
        session_id: &str,
        now: std::time::Instant,
    ) -> Option<PulseKind> {
        self.pulses
            .get(&(kind, session_id.to_string()))
            .and_then(|p| (now.duration_since(p.started) < PULSE_WINDOW).then_some(p.kind))
    }

    /// Whether any pulse is still lit (gates the fast repaint cadence).
    fn has_active_pulse(&self, now: std::time::Instant) -> bool {
        self.pulses
            .values()
            .any(|p| now.duration_since(p.started) < PULSE_WINDOW)
    }

    pub(crate) fn set_data_with_sessions(
        &mut self,
        mut agents: Vec<Agent>,
        panes: Vec<PaneInfo>,
        sessions: Vec<SessionInfo>,
        session_activity: Vec<SessionActivity>,
    ) {
        // Remember *which row* the cursor is on before the row set is
        // rebuilt from scratch below. Every refresh re-sorts, and the
        // default `sort = ["state", "session", "latest"]` reorders as
        // agents flip state, so holding the raw table index would silently
        // slide the highlight onto a neighbouring session.
        let selected = self.selected_identity();

        // Filter out paneless agents up front when the user has opted in
        // (the default). They can't be attached to from the picker — Enter
        // is a no-op — so listing them just clutters the actionable view.
        // The count is preserved on `paneless_hidden` so the footer can
        // surface a `+N paneless` hint and the rows aren't silently lost.
        self.paneless_hidden = 0;
        self.paneless_attention = 0;
        if self.watch_cfg.hide_paneless {
            let before = agents.len();
            // Background tasks are intentionally paneless but are the whole
            // point of being visible, so they're exempt from the hide.
            // Before dropping the rest, tally how many of them are blocked
            // on a human — a detached/SDK-hosted agent that goes
            // WaitingInput has no row and no attend target, so the header
            // attention summary is the only place it can surface.
            self.paneless_attention = agents
                .iter()
                .filter(|a| a.pane.is_none() && a.kind != AgentKind::Task)
                .filter(|a| agent_needs_attention(a.state))
                .count();
            agents.retain(|a| a.pane.is_some() || a.kind == AgentKind::Task);
            self.paneless_hidden = before - agents.len();
        }

        if self.watch_cfg.view != WatchView::Pane {
            self.rows = build_session_rows(
                agents,
                &panes,
                &sessions,
                &session_activity,
                &self.watch_cfg.sort,
            );
            self.panes = panes;
            self.sessions = sessions;
            self.session_activity = session_activity;
            self.last_refresh = OffsetDateTime::now_utc();
            self.restore_selection(selected);
            return;
        }

        // Sort agent rows according to the user's `[watch] sort` config.
        // Stale agents (pane already closed, i.e. lookup miss against the
        // panes inventory) always bucket at the end so live agents stay
        // visually grouped at the top regardless of the sort keys.
        //
        // Agent records carry only `pane_id`; session / window / pane
        // indices are resolved via the panes inventory collected this
        // refresh.
        let sort_context = SortContext::new(
            &panes,
            &sessions,
            &session_activity,
            OffsetDateTime::now_utc(),
        );
        let sort_keys = &self.watch_cfg.sort;
        agents.sort_by(|a, b| sort_agents(a, b, sort_keys, &sort_context));

        let known: HashSet<String> = agents.iter().filter_map(|a| a.pane.clone()).collect();

        let mut bare: Vec<PaneInfo> = panes
            .iter()
            .filter(|p| !known.contains(&p.pane_id))
            .cloned()
            .collect();
        bare.sort_by(|a, b| {
            a.session
                .cmp(&b.session)
                .then_with(|| a.window_index.cmp(&b.window_index))
                .then_with(|| a.pane_index.cmp(&b.pane_index))
        });

        let mut rows: Vec<WatchRow> = Vec::with_capacity(agents.len() + bare.len());
        rows.extend(agents.into_iter().map(WatchRow::agent));
        rows.extend(bare.into_iter().map(|p| WatchRow::BarePane(Box::new(p))));

        self.rows = rows;
        // Keep the *full* pane inventory (not just the bare ones) so
        // `pane_display` can resolve `session:window.pane` labels for
        // agent rows by lookup instead of a tmux shell-out per render.
        self.panes = panes;
        self.sessions = sessions;
        self.session_activity = session_activity;
        self.last_refresh = OffsetDateTime::now_utc();
        self.restore_selection(selected);
    }

    pub(crate) fn apply_sort_preset(&mut self, preset: WatchSortPreset) {
        self.watch_cfg.sort = preset.keys();
        self.resort_rows_preserving_selection();
    }

    /// Change granularity using the cached refresh payload, then keep the
    /// cursor on the same pane where possible. The next daemon refresh uses
    /// the new view automatically because `watch_cfg.view` is already set.
    pub(crate) fn apply_view(&mut self, view: WatchView) {
        if self.watch_cfg.view == view {
            return;
        }
        let selected_pane = self.selected_pane();
        let paneless_hidden = self.paneless_hidden;
        let paneless_attention = self.paneless_attention;
        let agents = self.current_agents();
        let panes = self.panes.clone();
        let sessions = self.sessions.clone();
        let session_activity = self.session_activity.clone();

        self.watch_cfg.view = view;
        self.columns = resolve_display_columns(&self.watch_cfg);
        self.expanded_sessions.clear();
        self.set_data_with_sessions(agents, panes, sessions, session_activity);
        // Hidden paneless agents are not present in `current_agents()`, so a
        // cache-only view rebuild cannot recount them. Preserve the counts
        // until the next full daemon refresh supplies the complete agent set.
        self.paneless_hidden = paneless_hidden;
        self.paneless_attention = paneless_attention;

        if let Some(pane_id) = selected_pane {
            if let Some(index) = self.visible_targets().iter().position(|target| {
                target_pane(self, *target).as_deref() == Some(pane_id.as_str())
                    || self.rows[target.row_idx].contains_pane(&pane_id)
            }) {
                self.table_state.select(Some(index));
                self.sync_auto_expansion();
            }
        }
    }

    fn resort_rows_preserving_selection(&mut self) {
        let selected = self.selected_identity();
        let sort_context = SortContext::new(
            &self.panes,
            &self.sessions,
            &self.session_activity,
            OffsetDateTime::now_utc(),
        );
        let sort_keys = &self.watch_cfg.sort;

        if self.watch_cfg.view == WatchView::Session {
            self.rows.sort_by(|a, b| match (a, b) {
                (WatchRow::Session(a), WatchRow::Session(b)) => {
                    sort_sessions(a, b, sort_keys, &sort_context)
                }
                (WatchRow::Session(_), _) => std::cmp::Ordering::Less,
                (_, WatchRow::Session(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });
        } else {
            self.rows.sort_by(|a, b| match (a, b) {
                (WatchRow::Agent(a), WatchRow::Agent(b)) => {
                    sort_agents(a, b, sort_keys, &sort_context)
                }
                (WatchRow::Agent(_), _) => std::cmp::Ordering::Less,
                (_, WatchRow::Agent(_)) => std::cmp::Ordering::Greater,
                (WatchRow::BarePane(a), WatchRow::BarePane(b)) => sort_panes(a, b),
                _ => std::cmp::Ordering::Equal,
            });
        }

        self.restore_selection(selected);
    }

    /// Identity of the row the cursor is on, if any.
    fn selected_identity(&self) -> Option<RowIdentity> {
        let target = self.selected_target()?;
        self.identity_for_target(target)
    }

    fn identity_for_target(&self, target: VisibleTarget) -> Option<RowIdentity> {
        if let Some(agent_idx) = target.agent_idx {
            let WatchRow::Session(session) = self.rows.get(target.row_idx)? else {
                return None;
            };
            let agent = session.agents.get(agent_idx)?;
            Some(RowIdentity::Agent(agent.kind, agent.session_id.clone()))
        } else {
            self.rows.get(target.row_idx).map(WatchRow::identity)
        }
    }

    fn session_group_for_identity(&self, identity: &RowIdentity) -> Option<String> {
        if self.watch_cfg.view != WatchView::Session {
            return None;
        }
        match identity {
            RowIdentity::Session(group_key) => self
                .rows
                .iter()
                .any(|row| {
                    matches!(
                        row,
                        WatchRow::Session(session)
                            if &session.group_key == group_key
                                && session.pane_count > 1
                                && !session.agents.is_empty()
                    )
                })
                .then(|| group_key.clone()),
            RowIdentity::Agent(kind, session_id) => self.rows.iter().find_map(|row| {
                let WatchRow::Session(session) = row else {
                    return None;
                };
                (session.pane_count > 1
                    && !session.agents.is_empty()
                    && session
                        .agents
                        .iter()
                        .any(|agent| agent.kind == *kind && agent.session_id == *session_id))
                .then(|| session.group_key.clone())
            }),
            RowIdentity::BarePane(_) => None,
        }
    }

    fn set_auto_expansion(&mut self, identity: Option<&RowIdentity>) {
        let group_key = identity.and_then(|id| self.session_group_for_identity(id));
        self.expanded_sessions.clear();
        if let Some(group_key) = group_key {
            self.expanded_sessions.insert(group_key);
        }
    }

    fn target_index_for_identity(&self, identity: &RowIdentity) -> Option<usize> {
        self.visible_targets().iter().position(|target| {
            self.identity_for_target(*target)
                .is_some_and(|candidate| &candidate == identity)
        })
    }

    /// Keep the selected session open without requiring a separate expand
    /// keystroke. Selecting one of its child agents keeps the same parent open;
    /// moving to another session folds the previous one and opens the new one.
    fn sync_auto_expansion(&mut self) {
        let identity = self.selected_identity();
        self.set_auto_expansion(identity.as_ref());
        if let Some(identity) = identity {
            if let Some(index) = self.target_index_for_identity(&identity) {
                self.table_state.select(Some(index));
            }
        }
    }

    /// Re-pin the cursor after `rows` was rebuilt or re-sorted.
    ///
    /// **Why identity and not the table index**: rows are re-sorted on
    /// every refresh (~2 Hz, plus one per pushed transition), and the
    /// default sort leads with `state`. When a *neighbouring* session
    /// starts working or goes blocked it jumps past the highlighted row,
    /// which shifts everything below it down one. Keeping the raw index
    /// then leaves the highlight sitting on a different session than the
    /// one the user aimed at — the "list looks shifted by one, Enter
    /// opened the wrong session" report. Pinning by identity makes the
    /// highlight travel *with* its row, so Enter always targets what the
    /// user is looking at.
    ///
    /// Falls back to the index clamp only when the row genuinely
    /// disappeared (session killed, agent exited).
    fn restore_selection(&mut self, previous: Option<RowIdentity>) {
        if let Some(prev) = previous.as_ref() {
            self.set_auto_expansion(Some(prev));
            if let Some(idx) = self.target_index_for_identity(prev) {
                self.table_state.select(Some(idx));
                return;
            }
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let targets = self.visible_targets();
        if targets.is_empty() {
            self.table_state.select(None);
            self.expanded_sessions.clear();
            return;
        }
        match self.table_state.selected() {
            Some(i) if i >= targets.len() => {
                self.table_state.select(Some(targets.len() - 1));
            }
            None => {
                // First non-empty load: prefer the row matching the pane the
                // user invoked muxa from, so the cursor lands on context.
                // `take()` ensures later refreshes don't re-snap selection.
                let hint = self.initial_pane.take();
                let initial = hint
                    .as_deref()
                    .and_then(|id| {
                        targets.iter().position(|target| {
                            target_pane(self, *target).as_deref() == Some(id)
                                || self.rows[target.row_idx].contains_pane(id)
                        })
                    })
                    .unwrap_or(0);
                self.table_state.select(Some(initial));
            }
            Some(_) => {}
        }
        self.sync_auto_expansion();
    }

    pub(crate) fn move_down(&mut self) {
        self.move_vertical(1);
    }

    pub(crate) fn move_up(&mut self) {
        self.move_vertical(-1);
    }

    fn move_first(&mut self) {
        self.move_to_vertical_boundary(false);
    }

    fn move_last(&mut self) {
        self.move_to_vertical_boundary(true);
    }

    fn move_page_down(&mut self) {
        self.move_vertical_bounded(self.page_step(false));
    }

    fn move_page_up(&mut self) {
        self.move_vertical_bounded(-self.page_step(false));
    }

    fn move_half_page_down(&mut self) {
        self.move_vertical_bounded(self.page_step(true));
    }

    fn move_half_page_up(&mut self) {
        self.move_vertical_bounded(-self.page_step(true));
    }

    fn page_step(&self, half: bool) -> isize {
        let rows = if half {
            (self.table_page_rows / 2).max(1)
        } else {
            self.table_page_rows.max(1)
        };
        isize::try_from(rows).unwrap_or(isize::MAX)
    }

    /// Move across session parents by default, even though the selected
    /// session's children are already visible. Once `→` enters a child, the
    /// same keys cycle only that session's visible children until `←` returns
    /// to the parent. This keeps a long child roster from slowing down the
    /// common case of scanning between sessions.
    fn move_vertical(&mut self, delta: isize) {
        let targets = self.visible_targets();
        if targets.is_empty() {
            return;
        }
        let selected = self.selected_visible_index(&targets);
        let candidates = self.vertical_candidates(&targets, selected);
        if candidates.is_empty() {
            return;
        }
        let current_position =
            selected.and_then(|index| candidates.iter().position(|candidate| *candidate == index));
        let next_position = match (current_position, delta.is_positive()) {
            (Some(position), true) => (position + 1) % candidates.len(),
            (Some(0), false) => candidates.len() - 1,
            (Some(position), false) => position - 1,
            (None, _) => 0,
        };
        self.table_state.select(Some(candidates[next_position]));
        self.sync_auto_expansion();
    }

    fn move_vertical_bounded(&mut self, delta: isize) {
        let targets = self.visible_targets();
        if targets.is_empty() {
            return;
        }
        let selected = self.selected_visible_index(&targets);
        let candidates = self.vertical_candidates(&targets, selected);
        if candidates.is_empty() {
            return;
        }
        let current = selected
            .and_then(|index| candidates.iter().position(|candidate| *candidate == index))
            .unwrap_or(0);
        let last = candidates.len() - 1;
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs()).min(last)
        } else {
            current.saturating_add(delta.unsigned_abs()).min(last)
        };
        self.table_state.select(Some(candidates[next]));
        self.sync_auto_expansion();
    }

    fn move_to_vertical_boundary(&mut self, last: bool) {
        let targets = self.visible_targets();
        if targets.is_empty() {
            return;
        }
        let selected = self.selected_visible_index(&targets);
        let candidates = self.vertical_candidates(&targets, selected);
        let next = if last {
            candidates.last()
        } else {
            candidates.first()
        };
        if let Some(index) = next {
            self.table_state.select(Some(*index));
            self.sync_auto_expansion();
        }
    }

    fn selected_visible_index(&self, targets: &[VisibleTarget]) -> Option<usize> {
        self.table_state
            .selected()
            .filter(|index| *index < targets.len())
    }

    fn vertical_candidates(
        &self,
        targets: &[VisibleTarget],
        selected: Option<usize>,
    ) -> Vec<usize> {
        let current = selected.and_then(|index| targets.get(index));
        if self.watch_cfg.view == WatchView::Session {
            match current {
                Some(target) if target.agent_idx.is_some() => targets
                    .iter()
                    .enumerate()
                    .filter_map(|(index, candidate)| {
                        (candidate.row_idx == target.row_idx && candidate.agent_idx.is_some())
                            .then_some(index)
                    })
                    .collect(),
                _ => targets
                    .iter()
                    .enumerate()
                    .filter_map(|(index, target)| target.agent_idx.is_none().then_some(index))
                    .collect(),
            }
        } else {
            (0..targets.len()).collect()
        }
    }

    fn move_into_session(&mut self) {
        self.sync_auto_expansion();
        let Some(index) = self.table_state.selected() else {
            return;
        };
        let targets = self.visible_targets();
        let Some(current) = targets.get(index) else {
            return;
        };
        if current.agent_idx.is_some() {
            return;
        }
        if targets
            .get(index + 1)
            .is_some_and(|next| next.row_idx == current.row_idx && next.agent_idx.is_some())
        {
            self.table_state.select(Some(index + 1));
            self.sync_auto_expansion();
        }
    }

    fn move_to_session_parent(&mut self) {
        let Some(target) = self.selected_target() else {
            return;
        };
        if target.agent_idx.is_none() {
            return;
        }
        let Some(WatchRow::Session(session)) = self.rows.get(target.row_idx) else {
            return;
        };
        self.restore_selection(Some(RowIdentity::Session(session.group_key.clone())));
    }

    /// `pane_id` of the currently selected row, if any.
    pub(crate) fn selected_pane(&self) -> Option<String> {
        target_pane(self, self.selected_target()?)
    }

    fn selected_target(&self) -> Option<VisibleTarget> {
        let index = self.table_state.selected()?;
        self.visible_targets().get(index).copied()
    }

    /// Borrow the currently-selected row, if any. Used by the quick-
    /// action handlers to decide whether the action is even applicable
    /// (e.g. `K` only applies to `WatchRow::Agent` rows with a
    /// non-`None` pane).
    pub(crate) fn selected_row(&self) -> Option<&WatchRow> {
        let target = self.selected_target()?;
        self.rows.get(target.row_idx)
    }

    fn selected_agent(&self) -> Option<&Agent> {
        let target = self.selected_target()?;
        match self.rows.get(target.row_idx)? {
            WatchRow::Agent(agent) => Some(agent),
            WatchRow::Session(session) => target
                .agent_idx
                .and_then(|idx| session.agents.get(idx))
                .or(session.latest_agent.as_ref()),
            WatchRow::BarePane(_) => None,
        }
    }

    /// `last_prompt` for the selected agent row, if it has one and
    /// the row is an `Agent` (bare panes have no prompt). Threaded
    /// through `c` to populate `QuickAction::CopyPrompt`.
    pub(crate) fn selected_last_prompt(&self) -> Option<&str> {
        self.selected_agent()?.last_prompt.as_deref()
    }

    fn visible_targets(&self) -> Vec<VisibleTarget> {
        let query = self.search_query.trim().to_lowercase();
        let mut targets = Vec::new();
        for (row_idx, row) in self.rows.iter().enumerate() {
            let row_query_match = query.is_empty() || row_matches_query(row, &self.panes, &query);
            let row_attention_match = !self.attention_only || row_needs_attention(row);
            if row_query_match && row_attention_match {
                targets.push(VisibleTarget {
                    row_idx,
                    agent_idx: None,
                });
            }

            let WatchRow::Session(session) = row else {
                continue;
            };
            if self.watch_cfg.view == WatchView::Swarm
                || session.pane_count <= 1
                || !self.expanded_sessions.contains(&session.group_key)
            {
                continue;
            }
            let session_name_match = query.is_empty()
                || contains_ci(&session.display_name, &query)
                || contains_ci(&session.session, &query);
            for (agent_idx, agent) in session.agents.iter().enumerate() {
                let query_match =
                    session_name_match || agent_matches_query(agent, &self.panes, &query);
                let attention_match = !self.attention_only || agent_needs_attention(agent.state);
                if query_match && attention_match {
                    targets.push(VisibleTarget {
                        row_idx,
                        agent_idx: Some(agent_idx),
                    });
                }
            }
        }
        targets
    }

    fn edit_search(&mut self, edit: impl FnOnce(&mut String)) {
        let selected = self.selected_identity();
        edit(&mut self.search_query);
        self.restore_selection(selected);
    }

    fn browse_keys_active(&self) -> bool {
        self.search_query.is_empty() && !self.explicit_search
    }

    fn arm_explicit_search(&mut self) {
        self.explicit_search = true;
        self.pending_g = false;
    }

    fn clear_search(&mut self) {
        self.explicit_search = false;
        self.edit_search(String::clear);
    }

    fn delete_search_word(&mut self) {
        self.edit_search(|query| {
            while query.chars().last().is_some_and(char::is_whitespace) {
                query.pop();
            }
            while query.chars().last().is_some_and(|c| !c.is_whitespace()) {
                query.pop();
            }
        });
    }

    fn toggle_attention_only(&mut self) {
        let selected = self.selected_identity();
        self.attention_only = !self.attention_only;
        self.restore_selection(selected);
    }

    fn toggle_event_inbox(&mut self) {
        self.event_inbox_open = !self.event_inbox_open;
        if self.event_inbox_open {
            self.unread_events = 0;
        }
    }

    /// Toggle the wide-screen inspector, and say which way it went.
    ///
    /// The hint carries real information here. The inspector is on by
    /// default, so the first `Alt-I` *hides* a panel rather than
    /// summoning one — and on a terminal under 120 columns nothing
    /// visibly changes either way, because the panel had no room to
    /// render. Both cases read as a dead key without a word from the
    /// footer, which is how a working binding gets reported as broken.
    fn toggle_inspector(&mut self) {
        self.inspector_enabled = !self.inspector_enabled;
        self.inspector_visible = false;
        self.pane_capture = None;
        self.set_hint(
            if self.inspector_enabled {
                "inspector enabled"
            } else {
                "inspector disabled"
            },
            HintLevel::Ok,
        );
    }

    /// Pre-format the pane label the way the user would read it in the
    /// table — `session:window.pane` when resolvable, raw `%id`
    /// otherwise. Lives on `App` because the panes inventory is here;
    /// `dispatch_quick_action` doesn't have access to it.
    pub(crate) fn pane_label(&self, pane_id: &str) -> String {
        pane_display(Some(pane_id), &self.panes)
    }

    /// Stamp a transient footer hint from the result of a quick action.
    /// The renderer reads this back and hides the default keybinding
    /// strip while the hint is fresh — see `FOOTER_HINT_TTL`.
    pub(crate) fn set_hint(&mut self, message: impl Into<String>, level: HintLevel) {
        self.footer_hint = Some(FooterHint {
            message: message.into(),
            level,
            set_at: Instant::now(),
        });
    }
}

fn target_pane(app: &App, target: VisibleTarget) -> Option<String> {
    match app.rows.get(target.row_idx)? {
        WatchRow::Session(session) => target
            .agent_idx
            .and_then(|idx| session.agents.get(idx))
            .and_then(|agent| agent.pane.clone())
            .or_else(|| session.representative_pane.clone()),
        row => row.pane_id().map(String::from),
    }
}

fn contains_ci(value: &str, lowercase_query: &str) -> bool {
    value.to_lowercase().contains(lowercase_query)
}

fn agent_matches_query(agent: &Agent, panes: &[PaneInfo], query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let pane_label = pane_display(agent.pane.as_deref(), panes);
    let kind = agent.kind.to_string();
    let state = agent.state.to_string();
    let matches = [
        Some(agent.session_id.as_str()),
        Some(kind.as_str()),
        Some(state.as_str()),
        Some(pane_label.as_str()),
        agent.tmux_session.as_deref(),
        agent.cwd.as_deref(),
        agent.model.as_deref(),
        agent.last_prompt.as_deref(),
        agent.last_response.as_deref(),
        agent.last_notification.as_deref(),
        agent.recap.as_deref(),
        agent.ai_title.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| contains_ci(value, query));
    matches
}

fn row_matches_query(row: &WatchRow, panes: &[PaneInfo], query: &str) -> bool {
    match row {
        WatchRow::Agent(agent) => agent_matches_query(agent, panes, query),
        WatchRow::BarePane(pane) => [
            pane.pane_id.as_str(),
            pane.session.as_str(),
            pane.current_command.as_str(),
            pane.title.as_str(),
            pane.current_path.as_str(),
        ]
        .into_iter()
        .any(|value| contains_ci(value, query)),
        WatchRow::Session(session) => {
            contains_ci(&session.display_name, query)
                || contains_ci(&session.session, query)
                || session
                    .bare_summary
                    .as_deref()
                    .is_some_and(|value| contains_ci(value, query))
                || session
                    .agents
                    .iter()
                    .any(|agent| agent_matches_query(agent, panes, query))
        }
    }
}

fn row_needs_attention(row: &WatchRow) -> bool {
    match row {
        WatchRow::Agent(agent) => agent_needs_attention(agent.state),
        WatchRow::Session(session) => session
            .agents
            .iter()
            .any(|agent| agent_needs_attention(agent.state)),
        WatchRow::BarePane(_) => false,
    }
}

struct SortContext<'a> {
    pane_by_id: HashMap<&'a str, &'a PaneInfo>,
    session_id_by_name: HashMap<&'a str, &'a str>,
    /// Display name for a group key. A group is keyed by `PaneInfo.session`:
    /// the session name on tmux, the `workspace_id` on herdr. Mapped from
    /// both `SessionInfo.name` and `SessionInfo.session_id` so a key resolves
    /// however the pane spells it — on tmux both entries point at the same
    /// name (identity display); on herdr the `workspace_id` entry surfaces
    /// the human label.
    display_by_key: HashMap<&'a str, &'a str>,
    activity_by_id: HashMap<&'a str, &'a SessionActivity>,
    now: OffsetDateTime,
}

impl<'a> SortContext<'a> {
    fn new(
        panes: &'a [PaneInfo],
        sessions: &'a [SessionInfo],
        session_activity: &'a [SessionActivity],
        now: OffsetDateTime,
    ) -> Self {
        let mut display_by_key = HashMap::new();
        for s in sessions {
            display_by_key.insert(s.name.as_str(), s.name.as_str());
            display_by_key.insert(s.session_id.as_str(), s.name.as_str());
        }
        Self {
            pane_by_id: panes.iter().map(|p| (p.pane_id.as_str(), p)).collect(),
            session_id_by_name: sessions
                .iter()
                .map(|s| (s.name.as_str(), s.session_id.as_str()))
                .collect(),
            display_by_key,
            activity_by_id: session_activity
                .iter()
                .map(|a| (a.session_id.as_str(), a))
                .collect(),
            now,
        }
    }

    fn pane(&self, pane_id: &str) -> Option<&'a PaneInfo> {
        self.pane_by_id.get(pane_id).copied()
    }

    /// The human-facing display name for a group key, falling back to the key
    /// itself when no session metadata matches (e.g. a stale pane).
    fn display_name(&self, session: &str) -> String {
        self.display_by_key
            .get(session)
            .copied()
            .unwrap_or(session)
            .to_string()
    }

    fn activity_for_session_name(&self, session: &str) -> Option<&'a SessionActivity> {
        // tmux keys panes by the mutable session *name* but the ledger by the
        // stable session *id*, so bridge name → id → activity. On herdr the
        // pane's session is already the stable `workspace_id` (= the ledger
        // key), so the name lookup misses and we fall back to keying the
        // ledger directly. The fallback is a no-op on tmux (session names
        // never collide with `$N` session ids) and only fires after a miss.
        self.session_id_by_name
            .get(session)
            .and_then(|id| self.activity_by_id.get(*id).copied())
            .or_else(|| self.activity_by_id.get(session).copied())
    }

    fn session_duration_secs(&self, session: &str) -> u64 {
        self.activity_for_session_name(session)
            .map_or(0, |a| a.effective_total_secs(self.now))
    }

    fn agent_session_duration_secs(&self, agent: &Agent) -> u64 {
        agent
            .pane
            .as_deref()
            .and_then(|id| self.pane(id))
            .map_or(0, |p| self.session_duration_secs(&p.session))
    }
}

/// Whether an agent state is blocked on a human — the same predicate the
/// notifier and `attend` use. Kept in sync with `state_sort_rank`'s top
/// three ranks.
fn agent_needs_attention(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

fn state_sort_rank(state: AgentState) -> u8 {
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

/// Compare two agents according to the user-configured sort keys.
///
/// Comparison flow (each step exits as soon as one agent is "less than"
/// the other — the rest are tiebreakers):
/// 1. Live agents always sort before stale agents (pane closed).
/// 2. Each `WatchSortKey` from the config, in order.
/// 3. `pane_id` lex ascending — final stable tiebreaker so the order is
///    deterministic across refreshes when every other key ties (matters
///    most for `Activity` when timestamps quantize to the same second).
fn sort_agents(
    a: &Agent,
    b: &Agent,
    keys: &[WatchSortKey],
    sort_context: &SortContext<'_>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let info_a = a.pane.as_deref().and_then(|id| sort_context.pane(id));
    let info_b = b.pane.as_deref().and_then(|id| sort_context.pane(id));

    // Stale (pane gone) → Ordering::Greater so it sinks to the bottom.
    match (info_a.is_some(), info_b.is_some()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    for key in keys {
        let cmp = match key {
            WatchSortKey::Session => info_a
                .map(|p| p.session.as_str())
                .cmp(&info_b.map(|p| p.session.as_str())),
            WatchSortKey::Activity => {
                // Reverse so newer (= larger timestamp) ends up first.
                b.last_activity_at.cmp(&a.last_activity_at)
            }
            WatchSortKey::State => state_sort_rank(a.state).cmp(&state_sort_rank(b.state)),
            WatchSortKey::SessionTime => sort_context
                .agent_session_duration_secs(b)
                .cmp(&sort_context.agent_session_duration_secs(a)),
            WatchSortKey::Pane => {
                let key_for = |info: Option<&PaneInfo>| {
                    info.map(|p| {
                        (
                            p.window_index.parse::<u32>().unwrap_or(u32::MAX),
                            p.pane_index.parse::<u32>().unwrap_or(u32::MAX),
                        )
                    })
                };
                key_for(info_a).cmp(&key_for(info_b))
            }
            WatchSortKey::PaneId => a.pane.as_deref().cmp(&b.pane.as_deref()),
        };
        if cmp != Ordering::Equal {
            return cmp;
        }
    }

    a.pane.as_deref().cmp(&b.pane.as_deref())
}

/// Host-namespaced grouping/identity key for a session row.
///
/// A tmux session and a herdr workspace can share a raw session id (both
/// named "w1"); grouping by the raw id alone merges them into one corrupted
/// row (wrong pane counts, mixed agents). Prefixing with the host keeps them
/// distinct. Ledger/display lookups still use the *raw* id (`session`), which
/// is unambiguous because each host's id-space is disjoint — this composite is
/// grouping/identity only. A row with no classifiable host (paneless agent,
/// `(no session)`, a background task) keys on the raw id, matching the
/// pre-multi-host grouping exactly.
fn session_group_key(host: Option<muxa::HostKind>, session: &str) -> String {
    match host {
        Some(muxa::HostKind::Tmux) => format!("tmux:{session}"),
        Some(muxa::HostKind::Herdr) => format!("herdr:{session}"),
        Some(muxa::HostKind::Zellij) => format!("zellij:{session}"),
        None => session.to_string(),
    }
}

/// Resolve an agent's session group: the raw session id (display/ledger key)
/// plus the pane's host namespace (grouping key input). Resolved from the
/// agent's pane when it's in the inventory; a paneless / stale agent has no
/// host, so it falls back to a raw synthetic session and `None`.
fn agent_session_group(
    agent: &Agent,
    pane_lookup: impl Fn(&str) -> Option<(String, Option<muxa::HostKind>)>,
) -> (String, Option<muxa::HostKind>) {
    agent
        .pane
        .as_deref()
        .and_then(&pane_lookup)
        .unwrap_or_else(|| {
            let session = match agent.pane.as_deref() {
                Some(p) => format!("(stale {p})"),
                // Paneless background tasks group under their own name.
                None if agent.kind == AgentKind::Task => agent.session_id.clone(),
                None => "(no session)".to_string(),
            };
            (session, None)
        })
}

fn build_session_rows(
    agents: Vec<Agent>,
    panes: &[PaneInfo],
    sessions: &[SessionInfo],
    session_activity: &[SessionActivity],
    sort_keys: &[WatchSortKey],
) -> Vec<WatchRow> {
    #[derive(Default)]
    struct Builder {
        /// Raw session id (display/ledger key).
        session: String,
        /// Host-namespaced grouping/identity key.
        group_key: String,
        panes: Vec<PaneInfo>,
        agents: Vec<Agent>,
        activity: Option<SessionActivity>,
    }

    let sort_context =
        SortContext::new(panes, sessions, session_activity, OffsetDateTime::now_utc());

    // Keyed by the host-namespaced `group_key`, not the raw session, so a tmux
    // session "w1" and a herdr workspace "w1" build two rows.
    let mut builders: HashMap<String, Builder> = HashMap::new();
    for p in panes {
        let host = muxa::backend::pane_id_host_kind(&p.pane_id);
        let key = session_group_key(host, &p.session);
        let entry = builders.entry(key.clone()).or_insert_with(|| Builder {
            session: p.session.clone(),
            group_key: key,
            ..Builder::default()
        });
        entry.panes.push(p.clone());
    }

    for agent in agents {
        let (session, host) = agent_session_group(&agent, |id| {
            sort_context.pane(id).map(|p| {
                (
                    p.session.clone(),
                    muxa::backend::pane_id_host_kind(&p.pane_id),
                )
            })
        });
        let key = session_group_key(host, &session);
        let entry = builders.entry(key.clone()).or_insert_with(|| Builder {
            session,
            group_key: key,
            ..Builder::default()
        });
        entry.agents.push(agent);
    }

    for builder in builders.values_mut() {
        if let Some(activity) = sort_context.activity_for_session_name(&builder.session) {
            builder.activity = Some(activity.clone());
        }
    }

    let mut rows: Vec<SessionRow> = builders
        .into_values()
        .map(|mut b| {
            b.panes.sort_by(sort_panes);
            b.agents.sort_by(|a, b| {
                b.last_activity_at
                    .cmp(&a.last_activity_at)
                    .then_with(|| a.session_id.cmp(&b.session_id))
            });
            let latest_agent = b.agents.first().cloned();
            let representative_pane = latest_agent
                .as_ref()
                .and_then(|a| a.pane.clone())
                .or_else(|| b.panes.first().map(|p| p.pane_id.clone()));
            let bare_summary = if latest_agent.is_none() {
                b.panes.first().map(|p| {
                    let first = if p.title.is_empty() || p.title == p.current_command {
                        p.current_command.clone()
                    } else {
                        format!("{}  {}", p.current_command, p.title)
                    };
                    if b.panes.len() > 1 {
                        format!("{first} · {} panes", b.panes.len())
                    } else {
                        first
                    }
                })
            } else {
                None
            };
            let agent_states = b
                .agents
                .iter()
                .map(|agent| ((agent.kind, agent.session_id.clone()), agent.state))
                .collect();
            SessionRow {
                display_name: sort_context.display_name(&b.session),
                group_key: b.group_key,
                session: b.session,
                pane_ids: b.panes.iter().map(|p| p.pane_id.clone()).collect(),
                representative_pane,
                latest_agent,
                agents: b.agents,
                pane_count: b.panes.len(),
                bare_summary,
                activity: b.activity,
                agent_states,
            }
        })
        .collect();

    rows.sort_by(|a, b| sort_sessions(a, b, sort_keys, &sort_context));
    rows.into_iter()
        .map(|row| WatchRow::Session(Box::new(row)))
        .collect()
}

/// The host a row belongs to, classified by its pane id's namespace
/// (`%…`→tmux, `herdr:…`→herdr, `zellij:…`→zellij). `None` when the row
/// carries no classifiable pane id (a paneless agent, a legacy/synthetic
/// id) — such rows get no host badge.
fn row_host(row: &WatchRow) -> Option<muxa::HostKind> {
    let pane_id = match row {
        WatchRow::Agent(a) => a.pane.as_deref(),
        WatchRow::BarePane(p) => Some(p.pane_id.as_str()),
        WatchRow::Session(s) => s
            .representative_pane
            .as_deref()
            .or_else(|| s.pane_ids.first().map(String::as_str)),
    }?;
    muxa::backend::pane_id_host_kind(pane_id)
}

/// Whether the visible row set spans more than one host — the trigger for
/// showing per-row host badges. Single-host users (the common case) see no
/// badge, so nothing changes for them. Rows with no classifiable host don't
/// count toward the distinct-host tally.
fn rows_multi_host(rows: &[WatchRow]) -> bool {
    let mut seen: Option<muxa::HostKind> = None;
    for row in rows {
        if let Some(host) = row_host(row) {
            match seen {
                Some(prev) if prev != host => return true,
                None => seen = Some(host),
                Some(_) => {}
            }
        }
    }
    false
}

/// The subtle dim host tag shown before a multi-host row's SESSION/PANE
/// cell. Mirrors the dashboard TUI's `CardHost` naming.
fn host_badge_label(host: muxa::HostKind) -> &'static str {
    match host {
        muxa::HostKind::Tmux => "tmux",
        muxa::HostKind::Zellij => "zellij",
        muxa::HostKind::Herdr => "herdr",
    }
}

/// Prepend the dim host tag to a cell's first line. Only called when the
/// row set spans multiple hosts, so the badge disambiguates rather than
/// adding noise.
fn prepend_host_badge(text: &mut Text<'_>, host: muxa::HostKind) {
    let style = Style::default().add_modifier(Modifier::DIM);
    let badge = Span::styled(format!("{} ", host_badge_label(host)), style);
    match text.lines.first_mut() {
        Some(line) => line.spans.insert(0, badge),
        None => text.lines.push(Line::from(badge)),
    }
}

fn prepend_tree_prefix(text: &mut Text<'_>, prefix: &str, style: Style) {
    let prefix = Span::styled(prefix.to_string(), style);
    match text.lines.first_mut() {
        Some(line) => line.spans.insert(0, prefix),
        None => text.lines.push(Line::from(prefix)),
    }
}

fn sort_panes(a: &PaneInfo, b: &PaneInfo) -> std::cmp::Ordering {
    a.session
        .cmp(&b.session)
        .then_with(|| {
            a.window_index
                .parse::<u32>()
                .unwrap_or(u32::MAX)
                .cmp(&b.window_index.parse::<u32>().unwrap_or(u32::MAX))
        })
        .then_with(|| {
            a.pane_index
                .parse::<u32>()
                .unwrap_or(u32::MAX)
                .cmp(&b.pane_index.parse::<u32>().unwrap_or(u32::MAX))
        })
        .then_with(|| a.pane_id.cmp(&b.pane_id))
}

fn sort_sessions(
    a: &SessionRow,
    b: &SessionRow,
    keys: &[WatchSortKey],
    sort_context: &SortContext<'_>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let pane_info = |row: &SessionRow| {
        row.representative_pane
            .as_deref()
            .and_then(|id| sort_context.pane(id))
    };
    for key in keys {
        let cmp = match key {
            WatchSortKey::Session => a.session.cmp(&b.session),
            WatchSortKey::Activity => b
                .latest_agent
                .as_ref()
                .map(|agent| agent.last_activity_at)
                .cmp(&a.latest_agent.as_ref().map(|agent| agent.last_activity_at)),
            WatchSortKey::State => {
                let rank = |row: &SessionRow| {
                    row.latest_agent
                        .as_ref()
                        .map_or(u8::MAX, |agent| state_sort_rank(agent.state))
                };
                rank(a).cmp(&rank(b))
            }
            WatchSortKey::SessionTime => sort_context
                .session_duration_secs(&b.session)
                .cmp(&sort_context.session_duration_secs(&a.session)),
            WatchSortKey::Pane => {
                let key_for = |info: Option<&PaneInfo>| {
                    info.map(|p| {
                        (
                            p.window_index.parse::<u32>().unwrap_or(u32::MAX),
                            p.pane_index.parse::<u32>().unwrap_or(u32::MAX),
                        )
                    })
                };
                key_for(pane_info(a)).cmp(&key_for(pane_info(b)))
            }
            WatchSortKey::PaneId => a.representative_pane.cmp(&b.representative_pane),
        };
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
    // Final tiebreak on the host-namespaced key so two rows sharing a raw
    // session id (tmux "w1" + herdr "w1") get a stable, deterministic order.
    a.group_key.cmp(&b.group_key)
}

/// Render a `pane_id` as `session:window.pane` when we can resolve it
/// against the cached pane list, falling back to the raw id (e.g.
/// `%1618`) when the agent's pane no longer exists.
///
/// **Why a slice and not a tmux shell-out**: this function runs once
/// per agent row per render frame. Shelling out to `tmux list-panes`
/// per call cost ~5 ms each, so a 35-agent table at 60 Hz target
/// blocked the input loop for ~175 ms per frame. The refresh task
/// already caches the full pane inventory in `App::panes`; we read
/// from there instead.
fn pane_display(pane_id: Option<&str>, panes: &[PaneInfo]) -> String {
    let Some(id) = pane_id else {
        // Distinguished from "—" / dash placeholders elsewhere so users
        // can see at a glance which agents have no tmux attachment to
        // jump into. Common case: Claude SDK sub-process whose env
        // didn't carry TMUX_PANE and whose process ancestry walk also
        // failed to find a pane.
        return "(no pane)".into();
    };
    match panes.iter().find(|p| p.pane_id == id) {
        Some(p) => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
        None => id.to_string(),
    }
}

const STATE_SUMMARY_ORDER: [AgentState; 7] = [
    AgentState::Error,
    AgentState::WaitingInput,
    AgentState::WaitingChoice,
    AgentState::Working,
    AgentState::Starting,
    AgentState::Idle,
    AgentState::Stopped,
];

const SESSION_STATE_GUTTER_WIDTH: usize = 6;
const SESSION_STATE_GUTTER_CONTENT_WIDTH: usize = SESSION_STATE_GUTTER_WIDTH - 1;

#[derive(Clone)]
struct StateSummaryPart {
    label: String,
    style: Style,
    count: usize,
}

fn state_summary_spans(
    states: impl IntoIterator<Item = AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<Span<'static>> {
    state_summary_parts(states, theme, spin)
        .into_iter()
        .enumerate()
        .flat_map(|(i, part)| {
            let mut spans = Vec::with_capacity(2);
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(part.label, part.style));
            spans
        })
        .collect()
}

fn state_summary_parts(
    states: impl IntoIterator<Item = AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<StateSummaryPart> {
    let states: Vec<AgentState> = states.into_iter().collect();
    let mut parts = Vec::new();

    for state in STATE_SUMMARY_ORDER {
        let count = states.iter().filter(|&&seen| seen == state).count();
        if count == 0 {
            continue;
        }
        let (symbol, style) = state_marker(state, theme, spin);
        let label = if count == 1 {
            symbol.to_string()
        } else {
            format!("{symbol}{count}")
        };
        parts.push(StateSummaryPart {
            label,
            style,
            count,
        });
    }

    parts
}

fn state_summary_parts_width(parts: &[StateSummaryPart]) -> usize {
    let labels = parts
        .iter()
        .map(|part| unicode_width::UnicodeWidthStr::width(part.label.as_str()))
        .sum::<usize>();
    let spaces = parts.len().saturating_sub(1);
    labels + spaces
}

fn overflow_label(count: usize, max_width: usize) -> String {
    let label = format!("+{count}");
    if unicode_width::UnicodeWidthStr::width(label.as_str()) <= max_width {
        label
    } else {
        "+".to_string()
    }
}

fn state_summary_gutter_spans(
    states: impl IntoIterator<Item = AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<Span<'static>> {
    let parts = state_summary_parts(states, theme, spin);
    let fitted = if state_summary_parts_width(&parts) <= SESSION_STATE_GUTTER_CONTENT_WIDTH {
        parts
    } else {
        let total_count = parts.iter().map(|part| part.count).sum::<usize>();
        let mut kept = Vec::new();
        let mut omitted_count = total_count;

        for (i, part) in parts.iter().enumerate() {
            let remaining_count = parts[i + 1..].iter().map(|part| part.count).sum::<usize>();
            let mut candidate = kept.clone();
            candidate.push(part.clone());
            let overflow_width = if remaining_count == 0 {
                0
            } else {
                let separator = usize::from(!candidate.is_empty());
                let label = overflow_label(remaining_count, SESSION_STATE_GUTTER_CONTENT_WIDTH);
                separator + unicode_width::UnicodeWidthStr::width(label.as_str())
            };
            if state_summary_parts_width(&candidate) + overflow_width
                <= SESSION_STATE_GUTTER_CONTENT_WIDTH
            {
                kept = candidate;
                omitted_count = remaining_count;
            } else {
                break;
            }
        }

        if omitted_count > 0 {
            kept.push(StateSummaryPart {
                label: overflow_label(omitted_count, SESSION_STATE_GUTTER_CONTENT_WIDTH),
                style: theme.dim_style(),
                count: omitted_count,
            });
        }
        kept
    };

    let mut spans = state_summary_spans_from_parts(&fitted);
    let width = spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    if width < SESSION_STATE_GUTTER_WIDTH {
        spans.push(Span::raw(" ".repeat(SESSION_STATE_GUTTER_WIDTH - width)));
    }
    spans
}

fn state_summary_spans_from_parts(parts: &[StateSummaryPart]) -> Vec<Span<'static>> {
    parts
        .iter()
        .enumerate()
        .flat_map(|(i, part)| {
            let mut spans = Vec::with_capacity(2);
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(part.label.clone(), part.style));
            spans
        })
        .collect()
}

fn session_label(s: &SessionRow, theme: WatchThemeSpec, spin: Spinner) -> Text<'static> {
    let mut spans = state_summary_gutter_spans(s.agent_states.values().copied(), theme, spin);
    spans.push(Span::raw(s.display_name.clone()));

    Text::from(Line::from(spans))
}

/// Per-frame animated state glyph for the `muxa watch` TUI. `enabled` mirrors
/// the `[watch] spinner` toggle; when off (and on every non-watch surface) the
/// static `state_icon` is used. Only `Working`/`Starting` animate.
#[derive(Clone, Copy)]
struct Spinner {
    frame: usize,
    enabled: bool,
}

impl Spinner {
    #[cfg(test)]
    const OFF: Spinner = Spinner {
        frame: 0,
        enabled: false,
    };

    fn glyph(self, state: AgentState) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        match state {
            AgentState::Working => Some(SWARM_DOTS[self.frame % SWARM_DOTS.len()]),
            AgentState::Starting => Some(SWARM_START[self.frame % SWARM_START.len()]),
            _ => None,
        }
    }
}

fn state_marker(state: AgentState, theme: WatchThemeSpec, spin: Spinner) -> (&'static str, Style) {
    // The static glyph is the shared source of truth with `muxa
    // status`/`status-line` (honoring the `[ui] icons` toggle); the spinner
    // only overrides `working`/`starting` inside the watch TUI.
    let icon = spin
        .glyph(state)
        .unwrap_or_else(|| crate::state_icon(state));
    (icon, theme.state_style(state))
}

fn state_age_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Error => "ERR",
        AgentState::WaitingChoice => "CHOICE",
        AgentState::WaitingInput => "WAIT",
        AgentState::Working => "WORK",
        AgentState::Starting => "START",
        AgentState::Idle => "IDLE",
        AgentState::Stopped => "STOP",
    }
}

fn state_age_text(
    agent: &Agent,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Text<'static> {
    let (symbol, style) = state_marker(agent.state, theme, spin);
    Text::from(Line::from(vec![
        Span::styled(format!("{symbol} "), style),
        Span::styled(
            format!(
                "{} {}",
                state_age_label(agent.state),
                relative_time(agent.state_entered_at, now)
            ),
            style,
        ),
    ]))
}

fn session_state_age_text(
    session: &SessionRow,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Text<'static> {
    let agent = session.agents.iter().min_by(|a, b| {
        state_sort_rank(a.state)
            .cmp(&state_sort_rank(b.state))
            .then_with(|| b.last_activity_at.cmp(&a.last_activity_at))
    });
    agent.map_or_else(
        || Text::from(Span::styled("—", theme.dim_style())),
        |agent| state_age_text(agent, now, theme, spin),
    )
}

/// A one-shot state-transition flash on an agent's State cell — a green `✓`
/// when a turn finishes (`working → idle`), a red flash when it errors. Unlike
/// the spinner (continuous, active states), a pulse marks a discrete *event*
/// and then decays to the static glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PulseKind {
    Done,
    Error,
}

#[derive(Clone, Copy)]
struct Pulse {
    kind: PulseKind,
    started: std::time::Instant,
}

/// How long a transition pulse stays lit before decaying to the static glyph.
const PULSE_WINDOW: Duration = Duration::from_millis(1300);

/// Classify a state transition into a pulse, if any. `prev == None` (first
/// sight, e.g. initial load) never pulses so the fleet doesn't flash on open.
fn pulse_kind(prev: Option<AgentState>, cur: AgentState) -> Option<PulseKind> {
    let prev = prev?;
    match cur {
        AgentState::Error if prev != AgentState::Error => Some(PulseKind::Error),
        AgentState::Idle if prev == AgentState::Working => Some(PulseKind::Done),
        _ => None,
    }
}

/// Glyph + style for an active pulse. Flashes via reverse-video on a ~2-frame
/// cadence, then the pulse expires and the cell reverts to its static glyph.
/// Honors `[ui] icons = ascii` for the `✓`.
fn pulse_glyph_style(kind: PulseKind, theme: WatchThemeSpec, frame: usize) -> (String, Style) {
    let (glyph, color) = match kind {
        PulseKind::Done => {
            let g = if icons_unicode() { "✓" } else { "+" };
            (g.to_string(), theme.state_working)
        }
        PulseKind::Error => (
            crate::state_icon(AgentState::Error).to_string(),
            theme.state_error,
        ),
    };
    let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    if (frame / 2).is_multiple_of(2) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    (glyph, style)
}

fn pulse_cell(kind: PulseKind, theme: WatchThemeSpec, frame: usize) -> Text<'static> {
    let (glyph, style) = pulse_glyph_style(kind, theme, frame);
    Text::from(Span::styled(glyph, style))
}

/// Active pulse per row (by index), resolved outside the render closure so it
/// borrows `app` immutably before the stateful table render takes
/// `&mut app.table_state`.
fn resolve_row_pulses(app: &App) -> Vec<Option<PulseKind>> {
    let now = std::time::Instant::now();
    app.rows
        .iter()
        .map(|r| {
            let who = match r {
                WatchRow::Agent(a) => Some((a.kind, a.session_id.as_str())),
                WatchRow::Session(s) => s
                    .latest_agent
                    .as_ref()
                    .map(|a| (a.kind, a.session_id.as_str())),
                WatchRow::BarePane(_) => None,
            };
            who.and_then(|(kind, sid)| app.active_pulse(kind, sid, now))
        })
        .collect()
}

/// True when any row holds a `working`/`starting` agent — i.e. something the
/// spinner would animate. Gates the fast repaint cadence so an all-idle fleet
/// keeps the calm 1 s idle repaint.
fn rows_have_active_spinner(rows: &[WatchRow]) -> bool {
    fn is_spinner_state(state: AgentState) -> bool {
        matches!(state, AgentState::Working | AgentState::Starting)
    }
    rows.iter().any(|r| match r {
        WatchRow::Agent(a) => is_spinner_state(a.state),
        WatchRow::Session(s) => s.agents.iter().any(|a| is_spinner_state(a.state)),
        WatchRow::BarePane(_) => false,
    })
}

fn session_time_text(s: &SessionRow, now: OffsetDateTime) -> Text<'static> {
    let Some(activity) = s.activity.as_ref() else {
        return Text::from(Span::styled(
            "-",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    };
    let text = format_duration(activity.effective_total_secs(now));
    let style = if activity.is_attached() {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Text::from(Span::styled(text, style))
}

fn format_duration(total_secs: u64) -> String {
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

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
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
            let _ = execute!(
                t.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                DisableBracketedPaste
            );
            let _ = t.show_cursor();
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste lets the terminal wrap pasted content in escape
    // markers so crossterm delivers it as a single `Event::Paste(String)`
    // — including any embedded newlines — instead of a stream of key
    // events where a pasted `\n` is indistinguishable from a real Enter.
    // The prompt composer relies on this to keep a multi-line paste from
    // submitting itself at the first newline.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// One result of a refresh handed back to the main loop. Two
/// shapes:
///
/// - `Full` — emitted by the periodic fallback tick (and the very
///   first refresh): full list of agents + pane inventory, replacing
///   the App's data. Catches up after `Lagged` drops on the
///   broadcast or when the stream reconnects.
/// - `SingleAgent` — emitted on every push from the subscribe
///   stream, carrying just the post-transition `Agent` payload. The
///   App applies it in place to the matching `(kind, session_id)`
///   row instead of replacing the whole list.
///
/// Routing transitions through `SingleAgent` is what stops the UI
/// from "redrawing every row on every tick": only the one row that
/// actually transitioned is touched. The fallback tick re-syncs
/// everything else periodically.
// `Full` is materially larger than `SingleAgent` (a Vec<Agent> + a
// Vec<PaneInfo>). Boxing the variant would make the enum smaller but
// adds an allocation per Full message; we send Full at most once per
// 5 s, so the size disparity is benign — silence clippy's
// large_enum_variant lint at the type definition.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RefreshOutcome {
    Full(FullRefresh),
    SingleAgent(Agent),
}

pub(crate) struct FullRefresh {
    pub agents: Vec<Agent>,
    pub panes: Vec<PaneInfo>,
    pub sessions: Vec<SessionInfo>,
    pub session_activity: Vec<SessionActivity>,
    pub error: Option<DaemonError>,
}

/// Apply a `RefreshOutcome` to `App`.
///
/// **Anti-flicker merge invariant** (added 2026-04-30): when a fresh
/// snapshot lands carrying a row whose state is `Starting` for an
/// `(kind, session_id)` we already track in a steady state
/// (`Working` / `Idle` / `WaitingInput` / `WaitingChoice` / `Error`
/// / `Stopped`), we
/// keep the previously-known state and only adopt the snapshot's
/// non-state fields (model, cost, `last_prompt`, …). Background:
///
///   - `state::Agent::new` initializes new entries to `Starting`,
///     and `Store::apply` only flips off `Starting` for events that
///     carry an explicit transition (`Started` → `Idle`,
///     `PromptSubmitted` → `Working`, etc.). Events that *don't*
///     change state (e.g. `ToolCompleted` against an `Idle` row, or
///     a `Heartbeat`) leave a freshly-inserted entry stuck in
///     `Starting` — and the v0.5.0 `Subscribe` push triggers a fresh
///     snapshot fetch on every transition, so any such transient
///     placeholder shows up in `muxa watch` as a single-tick row
///     that looks like "everything is Starting" relative to its
///     peers.
///   - The user-visible symptom: "한 순간에 STATE가 모두 starting으로
///     바뀌고 업데이트되는것같다" — the eye doesn't track which one row
///     blipped, only that the column "flickered".
///
/// The merge keeps Agent identity (matched by `(kind, session_id)`)
/// stable across refreshes, so `set_data`'s sort+selection logic
/// runs on a list that already reflects the row-level UI invariant.
/// New rows are appended (the snapshot's order is preserved before
/// `set_data` re-sorts). Gone rows simply don't appear in the new
/// snapshot and so drop.
pub(crate) fn apply_outcome(app: &mut App, outcome: RefreshOutcome) {
    apply_outcome_inner(app, outcome);
    // Diff the freshly-applied states against the prior snapshot to arm any
    // done/error transition pulses.
    app.detect_pulses();
}

fn apply_outcome_inner(app: &mut App, outcome: RefreshOutcome) {
    match outcome {
        RefreshOutcome::Full(full) => apply_full(app, full),
        RefreshOutcome::SingleAgent(agent) => {
            apply_single_agent(app, agent);
            // Re-sort immediately so a pushed state/activity change moves the
            // row to its sorted position now instead of waiting for the 5 s
            // `Full` fallback tick. This matters most for `sort = ["state",
            // …]`, where the sort key is itself a pushed field: without it the
            // badge updates instantly but the row stays put for up to 5 s.
            // The push already merged surgically into the existing rows
            // (`apply_single_agent`), and `sort_by` is stable, so only the row
            // that actually changed moves — this no longer reintroduces the
            // "all rows jumped" jitter that the original per-push full-snapshot
            // refresh caused. Selection is pinned by `pane_id`, so the cursor
            // stays on the same agent.
            app.resort_rows_preserving_selection();
        }
    }
}

fn merge_agent_for_ui(prior: &Agent, incoming: &Agent) -> Agent {
    // Preserve rich optional fields when the incoming payload carries
    // None. A Transition broadcast captures the Agent row exactly as it
    // exists after the event, but events that don't touch a field leave
    // it at its current value. The only way the payload has None is
    // when the row was freshly created (Starting placeholder) or the
    // event legitimately cleared the field. We distinguish the two
    // cases by keeping the UI's prior value when the new one is None —
    // a real clear would require an explicit Some("") or similar,
    // which no event produces today.
    let mut merged = incoming.clone();
    if merged.state == AgentState::Starting && prior.state != AgentState::Starting {
        merged.state = prior.state;
    }
    if merged.last_prompt.is_none() {
        merged.last_prompt.clone_from(&prior.last_prompt);
    }
    if merged.last_response.is_none() {
        merged.last_response.clone_from(&prior.last_response);
    }
    if merged.last_notification.is_none() {
        merged
            .last_notification
            .clone_from(&prior.last_notification);
    }
    if merged.workload.is_empty() {
        merged.workload.clone_from(&prior.workload);
    }
    if merged.model.is_none() {
        merged.model.clone_from(&prior.model);
    }
    if merged.context_used_pct.is_none() {
        merged.context_used_pct = prior.context_used_pct;
    }
    if merged.cost_usd.is_none() {
        merged.cost_usd = prior.cost_usd;
    }
    if merged.rate_limit_5h_pct.is_none() {
        merged.rate_limit_5h_pct = prior.rate_limit_5h_pct;
    }
    if merged.rate_limit_5h_resets_at.is_none() {
        merged.rate_limit_5h_resets_at = prior.rate_limit_5h_resets_at;
    }
    if merged.rate_limit_7d_pct.is_none() {
        merged.rate_limit_7d_pct = prior.rate_limit_7d_pct;
    }
    if merged.rate_limit_7d_resets_at.is_none() {
        merged.rate_limit_7d_resets_at = prior.rate_limit_7d_resets_at;
    }
    // NOTE: rate_limited_until, rate_limit_scope, and
    // rate_limit_source are intentionally NOT merged. Events like
    // `Started` and `TurnStopped` legitimately clear these fields (a
    // new session or a successful turn means the cap is lifted), and
    // preserving old values would make the UI show stale rate-limit
    // badges forever.
    merged
}

/// Apply a single push-driven `Transition.agent` to the matching
/// row in `app.rows`, leaving everything else untouched. If we don't
/// find a row for `(kind, session_id)`, append it — that's the
/// "first event for this session" case, where the next fallback
/// tick will reconcile sort order and pane labels.
fn apply_single_agent(app: &mut App, agent: Agent) {
    if app.watch_cfg.view == WatchView::Session {
        apply_single_agent_to_session(app, agent);
        return;
    }

    let key = (agent.kind, agent.session_id.clone());
    let mut updated = false;
    for row in &mut app.rows {
        if let WatchRow::Agent(a) = row {
            if (a.kind, a.session_id.clone()) == key {
                **a = merge_agent_for_ui(a, &agent);
                updated = true;
                break;
            }
        }
    }
    if !updated {
        app.rows.push(WatchRow::agent(agent));
    }
    // Caller (`apply_outcome`) re-sorts after this returns so a pushed change
    // reaches its sorted position without waiting for the 5 s `Full` tick. The
    // re-sort lives there (not here) so it runs once per outcome and the unit
    // tests that drive `apply_single_agent` directly can assert the pre-sort
    // merge in isolation.
}

fn apply_single_agent_to_session(app: &mut App, agent: Agent) {
    // Resolve the agent's raw session (display/ledger) and its pane host, then
    // match rows on the host-namespaced group key — the same keying
    // `build_session_rows` uses — so a herdr "w1" agent never merges into a
    // tmux "w1" row.
    let (session, host) = agent
        .pane
        .as_deref()
        .and_then(|id| {
            app.panes.iter().find(|p| p.pane_id == id).map(|p| {
                (
                    p.session.clone(),
                    muxa::backend::pane_id_host_kind(&p.pane_id),
                )
            })
        })
        .unwrap_or_else(|| {
            let session = agent
                .pane
                .as_deref()
                .map_or_else(|| "(no session)".to_string(), |p| format!("(stale {p})"));
            (session, None)
        });
    let group_key = session_group_key(host, &session);
    for row in &mut app.rows {
        let WatchRow::Session(s) = row else {
            continue;
        };
        if s.group_key != group_key {
            continue;
        }
        let key = (agent.kind, agent.session_id.clone());
        let mut updated_agent = agent.clone();
        if let Some(existing) = s
            .agents
            .iter_mut()
            .find(|a| (a.kind, a.session_id.clone()) == key)
        {
            updated_agent = merge_agent_for_ui(existing, &agent);
            *existing = updated_agent.clone();
        } else {
            s.agents.push(updated_agent.clone());
        }
        s.agents.sort_by(|a, b| {
            b.last_activity_at
                .cmp(&a.last_activity_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        if let Some(pane) = updated_agent.pane.as_ref() {
            if !s.pane_ids.iter().any(|id| id == pane) {
                s.pane_ids.push(pane.clone());
                s.pane_count = s.pane_count.max(s.pane_ids.len());
            }
        }
        s.agent_states.insert(key, updated_agent.state);
        s.latest_agent = s.agents.first().cloned();
        s.representative_pane = s
            .latest_agent
            .as_ref()
            .and_then(|a| a.pane.clone())
            .or_else(|| s.representative_pane.clone());
        return;
    }

    let mut agent_states = HashMap::new();
    agent_states.insert((agent.kind, agent.session_id.clone()), agent.state);
    // Resolve the workspace label from the last full refresh's session list
    // (herdr); on tmux and for an unknown key this is the key itself. The next
    // `Full` tick rebuilds the row via `build_session_rows` regardless.
    let display_name = app
        .sessions
        .iter()
        .find(|s| s.session_id == session || s.name == session)
        .map_or_else(|| session.clone(), |s| s.name.clone());
    app.rows.push(WatchRow::Session(Box::new(SessionRow {
        display_name,
        group_key,
        session,
        pane_ids: agent.pane.clone().into_iter().collect(),
        representative_pane: agent.pane.clone(),
        latest_agent: Some(agent.clone()),
        agents: vec![agent],
        pane_count: 0,
        bare_summary: None,
        activity: None,
        agent_states,
    })));
}

fn apply_full(app: &mut App, full: FullRefresh) {
    let FullRefresh {
        agents: mut new_agents,
        panes,
        sessions,
        session_activity,
        error,
    } = full;

    app.last_error = error;

    // Build a lookup of the previously-known agents so the merge can
    // distinguish a genuine daemon-driven change from a transient
    // `Starting` placeholder that also regressed optional fields to
    // None. In session view, this must include every collapsed agent,
    // not just the session's latest representative.
    let mut prev_rows: HashMap<(AgentKind, String), &Agent> = HashMap::new();
    for row in &app.rows {
        match row {
            WatchRow::Agent(a) => {
                prev_rows.insert((a.kind, a.session_id.clone()), a);
            }
            WatchRow::Session(s) => {
                for agent in &s.agents {
                    prev_rows.insert((agent.kind, agent.session_id.clone()), agent);
                }
            }
            WatchRow::BarePane(_) => {}
        }
    }

    for agent in &mut new_agents {
        let key = (agent.kind, agent.session_id.clone());
        if let Some(&prior) = prev_rows.get(&key) {
            *agent = merge_agent_for_ui(prior, agent);
        }
    }

    app.set_data_with_sessions(new_agents, panes, sessions, session_activity);
}

/// The "sessions" list for one host. tmux shells `list-sessions`; herdr
/// derives sessions from its workspaces over the socket (session id = raw
/// `workspace_id`, matching `PaneInfo.session` and the session-activity
/// ledger key so the DUR column resolves; display name = workspace label,
/// falling back to the id). zellij has no session concept here, so it stays
/// empty. May shell out or hit a socket — callers MUST run it off the tokio
/// runtime.
fn sessions_for_host(host: muxa::HostKind) -> Vec<SessionInfo> {
    match host {
        muxa::HostKind::Tmux => muxa::tmux::list_sessions().unwrap_or_default(),
        muxa::HostKind::Herdr => {
            let socket = muxa::backend::herdr::default_socket_path();
            muxa::backend::herdr::herdr_list_workspaces(&socket)
                .into_iter()
                .map(|ws| SessionInfo {
                    session_id: ws.id,
                    name: ws.label,
                    // herdr's socket API exposes no client-attach state
                    // (see docs/HERDR.md); watch never reads this field for
                    // display, and foreground time comes from the ledger.
                    attached_clients: 0,
                })
                .collect()
        }
        muxa::HostKind::Zellij => Vec::new(),
    }
}

/// Compute one refresh outcome: pane inventory aggregated across every
/// active backend (off-runtime via `spawn_blocking` so any shell-out
/// doesn't block the runtime) plus a daemon snapshot. Kept independent of
/// `App` so the work can run on a worker thread without holding any UI
/// state.
async fn compute_refresh(
    client: &Client,
    backends: &[muxa::SharedBackend],
    session_activity_path: Option<PathBuf>,
) -> RefreshOutcome {
    // Pane inventory is independent of the daemon — fetch it even when
    // muxad is down so `muxa watch` stays useful as a session picker. This
    // is the cross-multiplexer unified console: aggregate `list_panes` and
    // the per-host session sources across EVERY active backend so tmux and
    // herdr rows show side by side. Rows carry their host namespace in the
    // pane id, so a concat keeps them distinct.
    //
    // Each backend's `list_panes` / session source may shell out (tmux) or
    // hit a socket (herdr); neither may run on a tokio worker, so every one
    // goes through `spawn_blocking`. Spawning them up front runs the whole
    // fan-out concurrently — the tick budget is one host's latency, not the
    // sum (see docs/MULTI_HOST.md "Startup cost").
    let pane_tasks: Vec<_> = backends
        .iter()
        .map(|backend| {
            let backend = backend.clone();
            tokio::task::spawn_blocking(move || backend.list_panes())
        })
        .collect();
    let session_tasks: Vec<_> = backends
        .iter()
        .map(|backend| {
            let host = backend.kind();
            tokio::task::spawn_blocking(move || sessions_for_host(host))
        })
        .collect();

    let session_activity_task = async move {
        match session_activity_path {
            Some(path) => muxa::session_activity::load(&path).await,
            None => Vec::new(),
        }
    };

    // The blocking tasks already run concurrently on the blocking pool;
    // await the daemon snapshot + ledger load alongside them, then collect.
    let (session_activity, snapshot) = tokio::join!(session_activity_task, client.snapshot());

    let mut panes: Vec<PaneInfo> = Vec::new();
    for task in pane_tasks {
        panes.extend(task.await.unwrap_or_default());
    }
    let mut sessions: Vec<SessionInfo> = Vec::new();
    for task in session_tasks {
        sessions.extend(task.await.unwrap_or_default());
    }

    let full = match snapshot {
        Ok(agents) => FullRefresh {
            agents,
            panes,
            sessions,
            session_activity,
            error: None,
        },
        Err(e) => FullRefresh {
            agents: Vec::new(),
            panes,
            sessions,
            session_activity,
            error: Some(DaemonError {
                self_describing: matches!(e, RuntimeError::NotConnected(_)),
                message: e.to_string(),
            }),
        },
    };
    RefreshOutcome::Full(full)
}

/// Background task that owns its own `Client` clone and produces refresh
/// outcomes on a 500 ms tick or whenever the input loop sends a wake
/// request. The task exits cleanly when *either* end of either channel
/// closes — main drops `wake_tx` to signal shutdown.
///
/// Generic over the fetcher so unit tests can swap in a closure that
/// returns a canned `RefreshOutcome` without touching tmux or the daemon.
async fn refresh_task<F, Fut, S>(
    mut fetch: F,
    mut wake: mpsc::Receiver<()>,
    out: mpsc::Sender<RefreshOutcome>,
    sub_init: S,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = RefreshOutcome> + Send,
    S: Future<Output = Option<muxa::ipc::TransitionStream>> + Send + 'static,
{
    // Acquire the streaming subscription as the first thing the
    // background task does — `run` doesn't await it any more, so the
    // popup gets to paint its empty frame before we pay this latency.
    // `None` falls back to historical polling.
    let mut sub = sub_init.await;

    // When we have a streaming subscription, push updates handle the
    // common case in milliseconds — the polling tick only exists for
    // catch-up after `Lagged` drops or reconnect. Without the
    // subscription we fall back to the historical 500 ms cadence so
    // the watch still updates against an old daemon.
    let interval_dur = if sub.is_some() {
        STREAMING_FALLBACK_INTERVAL
    } else {
        POLL_INTERVAL
    };
    let mut tick = tokio::time::interval(interval_dur);
    // If a refresh runs longer than one tick (slow daemon, slow tmux),
    // don't pile up backlog ticks — skip them.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first `tick()` fires immediately. We don't want a duplicate
    // refresh right after the priming snapshot in `run`, so consume it.
    tick.tick().await;

    loop {
        // Reduce subscribe-arm noise: the `if sub.is_some()` guard
        // on the select branch keeps the helper out of the wait set
        // entirely when we don't have a stream — `pending()` would
        // never resolve but tokio still polls it once per loop, which
        // would burn a tiny bit of CPU and obscure traces.
        tokio::select! {
            _ = tick.tick() => {
                // Periodic full sync — catches up after lagged drops
                // or any state we missed via the push stream.
                let outcome = fetch().await;
                if out.send(outcome).await.is_err() {
                    return;
                }
            }
            req = wake.recv() => {
                // None => the input loop dropped wake_tx, i.e. quit/attach.
                if req.is_none() {
                    return;
                }
                // User-triggered (`r` key, or the priming wake from
                // `run`) → also a full sync.
                let outcome = fetch().await;
                if out.send(outcome).await.is_err() {
                    return;
                }
            }
            res = recv_transition(&mut sub), if sub.is_some() => {
                if let Some(agent) = res {
                    // Push-driven update: ship just the changed
                    // row to the main loop. Avoids the "every tick
                    // redraws every row" jitter where the full
                    // snapshot would replace LAST PROMPT / STATE /
                    // etc. for every agent on every transition.
                    if out
                        .send(RefreshOutcome::SingleAgent(agent))
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else {
                    // Daemon closed the stream OR parse/IO error.
                    // Drop the subscription and continue with pure
                    // polling so the user keeps a working (if
                    // higher-latency) view.
                    sub = None;
                    tick = tokio::time::interval(POLL_INTERVAL);
                    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    tick.tick().await;
                }
            }
        }
    }
}

/// Helper for the select arm: await the next transition. Returns
/// `Some(agent)` when a push lands (the post-transition payload from
/// the daemon), and `None` when the stream is dead — caller falls
/// back to polling on `None`.
async fn recv_transition(sub: &mut Option<muxa::ipc::TransitionStream>) -> Option<Agent> {
    let stream = sub.as_mut()?;
    match stream.recv().await {
        // The wire payload deserializes as `Arc<Agent>` (the producer
        // wraps once to make the broadcast fanout O(refcount) instead
        // of O(sizeof(Agent))). On the client side the strong count is
        // always 1 — this is a fresh `Arc` built by `serde` — so
        // `Arc::try_unwrap` is guaranteed to succeed and avoids a
        // pointless `Agent` clone here.
        Ok(Some(t)) => Some(std::sync::Arc::try_unwrap(t.agent).unwrap_or_else(|a| (*a).clone())),
        Ok(None) | Err(_) => None,
    }
}

/// Entry point for `muxa watch`.
///
/// Returns `Some(pane_id)` if the user pressed Enter on a selected agent,
/// meaning they want to attach to that pane. The caller (`main.rs`) runs
/// the actual tmux switch-client invocation *after* this returns so the
/// terminal is already restored by the time we hand off control.
#[allow(clippy::too_many_lines)] // mostly setup + action dispatch — extracting a helper
                                 // for three preview-related arms hurts readability more than it helps
pub async fn run(
    client: &Client,
    watch_cfg: WatchConfig,
    session_activity_path: Option<PathBuf>,
    activity_path: Option<PathBuf>,
    sort_persist_path: Option<PathBuf>,
    caller_pane: Option<String>,
) -> Result<Option<String>> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);

    let mut app = App::with_config(watch_cfg);
    // The unified console observes every active host. The set threads
    // through the priming refresh and the background task; the live
    // capture path resolves a backend per pane-id namespace instead.
    let backends: Vec<muxa::SharedBackend> = muxa::active_backends();

    // "Where am I" is inherently single-host (env-based) — land the cursor on
    // the user's current pane on first load. `current_pane` is env-based: the
    // env-preferred backend answers inside its own pane, but inside e.g. a
    // herdr pane launched from tmux BOTH backends could answer. `active_backends`
    // is ordered by env preference (`backends[0]` = the env-preferred host), so
    // we resolve in set order and take the first `Some` — env preference breaks
    // the tie. Single-host: identical (the one backend answers or doesn't).
    // The binding-expanded pane beats anything derived in-process: it was
    // resolved by tmux at the keypress, in the pressing client's context,
    // which no query made from inside a popup can reproduce.
    let initial_pane = caller_pane.or_else(|| backends.iter().find_map(|b| b.current_pane()));
    app.set_initial_pane(initial_pane.clone());
    app.collaboration.origin = watch_collaboration_origin(initial_pane.clone());
    let watch_started_at = OffsetDateTime::now_utc();

    // Paint the first frame **before** any IPC. The popup
    // (`prefix + s` → `display-popup -E muxa watch`) becomes visible
    // the instant tmux finishes spawning us, so we want the user to
    // see the table scaffold (header + empty body) immediately
    // rather than a black rectangle for the ~50-100 ms it takes to
    // shell out to `tmux list-panes` and round-trip a snapshot.
    //
    // The first real refresh fires from the background task right
    // after this — we send a wake on the channel below so it doesn't
    // wait for the 5 s fallback tick.
    guard
        .terminal_mut()
        .draw(|f| render(f, &mut app))
        .map_err(anyhow::Error::from)?;
    refresh_watch_collaboration(client, &mut app).await;

    // Background refresh task owns its own Client clone so the borrowed
    // `client: &Client` doesn't have to outlive the task. The clone is
    // cheap (a single `PathBuf`) and avoids needing an `Arc`/lifetime
    // wrapper for what is effectively immutable data. The backend is
    // already an `Arc<dyn …>` so cloning it is just a refcount bump.
    let bg_client = client.clone();
    let bg_backends = backends.clone();
    let bg_session_activity_path = session_activity_path.clone();
    let sub_client = client.clone();
    let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    let (outcome_tx, mut outcome_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);

    // Subscribe lazily inside the refresh task so its `await` doesn't
    // block the first paint. Falls back to polling on any error
    // (older daemon, socket unreachable, connection refused).
    let subscription_init = async move {
        match sub_client.subscribe().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(error = %e, "subscribe unavailable; falling back to polling");
                None
            }
        }
    };

    let bg = tokio::spawn(refresh_task(
        move || {
            let client = bg_client.clone();
            let backends = bg_backends.clone();
            let session_activity_path = bg_session_activity_path.clone();
            async move { compute_refresh(&client, &backends, session_activity_path).await }
        },
        wake_rx,
        outcome_tx,
        subscription_init,
    ));

    // Force the priming refresh immediately so the empty frame above
    // gets replaced by real data within ~50-100 ms instead of waiting
    // for the next fallback tick. `try_send` is safe here — the
    // channel is freshly created with capacity > 0, so the send can't
    // block or fail.
    let _ = wake_tx.try_send(());

    let mut jump_target: Option<String> = None;

    // Repaint only when something actually changed (`needs_render`) or the
    // idle cadence elapsed, instead of once per input-poll tick. `true` here
    // forces the first in-loop frame; `Instant::now()` seeds the cadence.
    let mut needs_render = true;
    let mut last_render = std::time::Instant::now();

    loop {
        // Animate on a fast cadence only while there's something to move:
        // the swarm view always, or the table views when `[watch] spinner`
        // is on and at least one agent is working/starting — or while a
        // transition pulse is still lit. An otherwise-idle fleet falls back to
        // the calm 1 s idle repaint.
        let spinning = (app.watch_cfg.view == WatchView::Swarm || app.watch_cfg.spinner)
            && icons_unicode()
            && rows_have_active_spinner(&app.rows);
        let animating = spinning || app.has_active_pulse(std::time::Instant::now());
        let redraw_interval = if animating {
            SWARM_REDRAW_INTERVAL
        } else {
            IDLE_REDRAW_INTERVAL
        };
        if needs_render || last_render.elapsed() >= redraw_interval {
            guard
                .terminal_mut()
                .draw(|f| render(f, &mut app))
                .map_err(anyhow::Error::from)?;
            last_render = std::time::Instant::now();
            needs_render = false;
        }

        // Drain every event already in the OS buffer in one go before
        // rendering again. Holding a key (or typing in bursts) used to
        // pile up events because the loop only handled one per
        // iteration; the render between events made the queue grow
        // faster than we drained. `poll(Duration::ZERO)` is
        // non-blocking, so this is cheap when the buffer is empty.
        let mut events = drain_pending_events()?;

        // If nothing was waiting, do exactly one bounded blocking
        // wait so an idle UI yields the CPU.
        if events.is_empty() {
            let waited = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
                if crossterm::event::poll(INPUT_POLL)? {
                    Ok(Some(crossterm::event::read()?))
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(anyhow::Error::from)??;
            if let Some(ev) = waited {
                events.push(ev);
            }
        }

        // Any input (key, resize, mouse) may change what's on screen, so
        // repaint next pass. An event that turns out to be a no-op costs one
        // extra render — negligible next to skipping the idle ~62 fps redraw.
        if !events.is_empty() {
            needs_render = true;
        }

        let mut quit = false;
        for ev in events {
            match handle_event(ev, &mut app) {
                Action::Quit => {
                    quit = true;
                    break;
                }
                Action::AttachPane(pane) => {
                    jump_target = Some(pane);
                    quit = true;
                    break;
                }
                Action::Refresh => {
                    // Coalesce repeated `r` mashes: if the wake slot is
                    // already full a request is pending, so a `try_send`
                    // failure is fine — the in-flight request will pick
                    // up the user's intent.
                    let _ = wake_tx.try_send(());
                    app.refresh_pending = true;
                    refresh_watch_collaboration(client, &mut app).await;
                }
                Action::OpenPreview => {
                    if let Some(pane_id) = app.selected_pane() {
                        app.preview = Some(PreviewState {
                            pane_id,
                            scroll: 0,
                            mode: PreviewMode::Popup,
                            // Honor `[watch.preview] default_content` so
                            // first-paint shape (live pane vs prompt
                            // text) matches the user's preference. `c`
                            // still toggles in either direction at runtime.
                            content: app.watch_cfg.preview.default_content,
                        });
                    }
                }
                Action::ClosePreview => {
                    app.preview = None;
                    // Drop any cached pane capture — keeping it would
                    // pin a stale snapshot in memory across preview
                    // sessions and might leak across tmux pane reuse
                    // within the same `pane_id`.
                    app.pane_capture = None;
                }
                Action::TogglePreviewMode => {
                    if let Some(p) = app.preview.as_mut() {
                        p.mode = match p.mode {
                            PreviewMode::Popup => PreviewMode::Fullscreen,
                            PreviewMode::Fullscreen => PreviewMode::Popup,
                        };
                    }
                }
                Action::TogglePreviewContent => {
                    if let Some(p) = app.preview.as_mut() {
                        p.content = match p.content {
                            PreviewContent::PromptResponse => PreviewContent::LivePane,
                            PreviewContent::LivePane => PreviewContent::PromptResponse,
                        };
                        // Reset scroll so the new content starts from
                        // the top — re-using the prompt-mode scroll
                        // offset on a wholly-different content surface
                        // tends to land mid-line.
                        p.scroll = 0;
                        // Drop the cache when leaving LivePane so the
                        // next entry starts with a fresh capture.
                        if matches!(p.content, PreviewContent::PromptResponse) {
                            app.pane_capture = None;
                        }
                    }
                }
                Action::SetSort(preset) => {
                    app.apply_sort_preset(preset);
                    match sort_persist_path.as_deref() {
                        Some(path) => match persist_watch_sort(path, &app.watch_cfg.sort) {
                            Ok(()) => {
                                app.set_hint(
                                    format!("sorted by {} (saved)", preset.label()),
                                    HintLevel::Ok,
                                );
                            }
                            Err(e) => {
                                app.set_hint(
                                    format!("sorted by {} (save failed: {e})", preset.label()),
                                    HintLevel::Warn,
                                );
                            }
                        },
                        None => {
                            app.set_hint(format!("sorted by {}", preset.label()), HintLevel::Ok);
                        }
                    }
                }
                Action::SetView(view) => {
                    app.apply_view(view);
                    let label = match view {
                        WatchView::Pane => "pane",
                        WatchView::Session => "session",
                        WatchView::Swarm => "swarm",
                    };
                    app.set_hint(format!("view: {label}"), HintLevel::Ok);
                }
                Action::AskConfirm(popup) => {
                    app.confirm = Some(popup);
                }
                Action::OpenCollaborationMessage => {
                    refresh_watch_collaboration(client, &mut app).await;
                    open_watch_collaboration_composer(&mut app);
                }
                Action::OpenCollaborationMailbox => {
                    refresh_watch_collaboration(client, &mut app).await;
                    app.collaboration_mailbox.open = true;
                    clamp_collaboration_mailbox(&mut app);
                }
                Action::SubmitCollaboration => {
                    if let Some(composer) = app.collaboration_composer.take() {
                        let outcome = run_watch_collaboration_composer(client, composer).await;
                        apply_outcome_to_app(&mut app, outcome);
                        refresh_watch_collaboration(client, &mut app).await;
                    }
                }
                Action::CancelCollaborationComposer => {
                    app.collaboration_composer = None;
                }
                Action::ClaimCollaborationInbox => {
                    let outcome = match app.collaboration.origin.clone() {
                        Some(origin) => match client.collaboration_inbox(&origin).await {
                            Ok(requests) if requests.is_empty() => {
                                ActionOutcome::Ok("collaboration inbox is empty".into())
                            }
                            Ok(requests) => ActionOutcome::Ok(format!(
                                "claimed {} collaboration request{}",
                                requests.len(),
                                if requests.len() == 1 { "" } else { "s" }
                            )),
                            Err(error) => ActionOutcome::Err(format!("inbox failed: {error}")),
                        },
                        None => ActionOutcome::Err(collaboration_open_hint().into()),
                    };
                    apply_outcome_to_app(&mut app, outcome);
                    refresh_watch_collaboration(client, &mut app).await;
                    app.collaboration_mailbox.tab = CollaborationMailboxTab::Incoming;
                    clamp_collaboration_mailbox(&mut app);
                }
                Action::ConfirmYes => {
                    if let Some(popup) = app.confirm.take() {
                        // The confirmed action (kill-pane, abort-turn, or a
                        // prompt send) shells out to tmux synchronously —
                        // and the send path also grace-sleeps between the
                        // text and the Enter. Run the whole thing on a
                        // blocking worker so neither the tmux fork nor the
                        // sleep stalls the input/render loop.
                        let outcome = tokio::task::spawn_blocking(move || {
                            let mut fx = RealEffects;
                            dispatch_quick_action(popup.on_confirm, &mut fx)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            ActionOutcome::Err(format!("✗ action task failed: {e}"))
                        });
                        apply_outcome_to_app(&mut app, outcome);
                    }
                }
                Action::ConfirmCancel => {
                    app.confirm = None;
                }
                Action::Quick(qa) => {
                    if matches!(qa, QuickAction::ShowHelp) {
                        app.help_open = !app.help_open;
                    } else {
                        // Copy shells out to clipboard helpers (and can fall
                        // back to a /tmp write) — off the loop thread it
                        // goes, same as the confirmed destructive actions.
                        let outcome = tokio::task::spawn_blocking(move || {
                            let mut fx = RealEffects;
                            dispatch_quick_action(qa, &mut fx)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            ActionOutcome::Err(format!("✗ action task failed: {e}"))
                        });
                        apply_outcome_to_app(&mut app, outcome);
                    }
                }
                Action::NotApplicable(msg) => {
                    app.set_hint(msg, HintLevel::Warn);
                }
                Action::None => {}
            }
        }

        // Live pane capture: the modal live preview and the wide-screen
        // inspector share one cache. When either is visible and the cache is
        // missing or stale (>500 ms), call into
        // the active backend on a worker thread. Bounded by the
        // existing 500 ms TTL so we never fork more than ~2 Hz,
        // regardless of how fast the input loop spins. Capability-
        // gated: backends that report `caps().capture_pane == false`
        // (zellij CLI today) skip the call and the renderer shows a
        // "(not supported)" placeholder.
        let capture_pane = app
            .preview
            .as_ref()
            .filter(|preview| preview.content == PreviewContent::LivePane)
            .map(|preview| preview.pane_id.clone())
            .or_else(|| app.inspector_visible.then(|| app.selected_pane()).flatten());
        if let Some(capture_pane) = capture_pane {
            // Resolve the capturing backend by the pane id's namespace so a
            // herdr row captures via herdr even when tmux is the primary
            // host (and vice versa). Cheap to build per capture (bounded to
            // ~2 Hz by the TTL below).
            let cap_backend = crate::backend_for_pane(&capture_pane);
            if cap_backend.caps().capture_pane {
                let stale = app.pane_capture.as_ref().is_none_or(|c| {
                    c.pane_id != capture_pane
                        || c.fetched_at.elapsed() >= Duration::from_millis(500)
                });
                if stale {
                    let pane_id = capture_pane.clone();
                    let captured =
                        tokio::task::spawn_blocking(move || cap_backend.capture_pane(&pane_id))
                            .await
                            .ok()
                            .flatten();
                    app.pane_capture = Some(CapturedPane {
                        pane_id: capture_pane,
                        text: captured.unwrap_or_default(),
                        fetched_at: std::time::Instant::now(),
                    });
                    // Fresh preview bytes — repaint to show them.
                    needs_render = true;
                }
            }
        }

        // Drain any refresh outcomes that landed since the last frame.
        // The render path never awaits the refresh — this is the only
        // place data flows back into `App`.
        let mut received_outcome = false;
        while let Ok(outcome) = outcome_rx.try_recv() {
            apply_outcome(&mut app, outcome);
            received_outcome = true;
        }
        if received_outcome {
            app.refresh_pending = false;
            needs_render = true;
        }

        if quit {
            break;
        }
    }

    let watch_pane = app.selected_pane().or(initial_pane);
    append_human_interaction(
        activity_path.as_deref(),
        HumanInteractionKind::MuxaWatch,
        &app,
        watch_pane.as_deref(),
        watch_started_at,
        OffsetDateTime::now_utc(),
    )
    .await;

    // Drop the wake sender so refresh_task's `wake.recv()` returns None
    // on its next iteration; then await the join so we don't leak a task.
    //
    // Bound the join with a short timeout: if the refresh task is mid-
    // `tmux list-panes` shell-out or stuck on a daemon snapshot to a
    // hung Unix socket, we'd otherwise wedge `muxa watch` quit until
    // those resolve at the OS layer. 2 seconds is well above the
    // usual fetch latency (sub-50 ms) so a healthy run never hits it.
    drop(wake_tx);
    drop(outcome_rx);
    if tokio::time::timeout(Duration::from_secs(2), bg)
        .await
        .is_err()
    {
        tracing::warn!("refresh task did not exit within 2s of shutdown; abandoning");
    }

    Ok(jump_target)
}

fn watch_collaboration_origin(initial_pane: Option<String>) -> Option<CollaborationOrigin> {
    watch_collaboration_origin_from(initial_pane, std::env::var("TMUX").ok())
}

fn watch_collaboration_origin_from(
    initial_pane: Option<String>,
    tmux: Option<String>,
) -> Option<CollaborationOrigin> {
    let pane = initial_pane.filter(|pane| pane.starts_with('%'))?;
    let socket = tmux.and_then(|value| {
        let path = value.split(',').next()?.trim();
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    });
    Some(CollaborationOrigin { pane, socket })
}

fn collaboration_open_hint() -> &'static str {
    "collaboration unavailable here — focus an agent pane and open muxa watch with prefix+s"
}

fn friendly_watch_collaboration_error(error: &str) -> String {
    if error.contains("collaboration origin is not a tracked pane agent") {
        collaboration_open_hint().into()
    } else {
        error.into()
    }
}

async fn refresh_watch_collaboration(client: &Client, app: &mut App) {
    let Some(origin) = app.collaboration.origin.clone() else {
        app.collaboration = WatchCollaboration {
            unavailable: Some(collaboration_open_hint().into()),
            ..WatchCollaboration::default()
        };
        return;
    };
    let room = match client.collaboration_context(&origin).await {
        Ok(room) => room,
        Err(error) => {
            app.collaboration = WatchCollaboration {
                origin: Some(origin),
                unavailable: Some(friendly_watch_collaboration_error(&error.to_string())),
                ..WatchCollaboration::default()
            };
            return;
        }
    };
    let (incoming, sent) = tokio::join!(
        client.collaboration_list(&origin, RequestMailbox::Incoming),
        client.collaboration_list(&origin, RequestMailbox::Sent),
    );
    match (incoming, sent) {
        (Ok(incoming), Ok(sent)) => {
            app.collaboration = WatchCollaboration {
                origin: Some(origin),
                room: Some(room),
                incoming,
                sent,
                unavailable: None,
            };
        }
        (incoming, sent) => {
            let error = incoming
                .err()
                .or_else(|| sent.err())
                .expect("at least one mailbox request failed");
            app.collaboration = WatchCollaboration {
                origin: Some(origin),
                room: Some(room),
                unavailable: Some(format!("mailbox unavailable: {error}")),
                ..WatchCollaboration::default()
            };
        }
    }
    clamp_collaboration_mailbox(app);
}

/// A request needs a peer; keystrokes only need a pane. When the full
/// composer cannot open, `m` degrades to the keystrokes-only form against
/// the selected pane instead of refusing outright, and the reason the
/// contract modes are missing rides along as a warning hint.
fn open_prompt_only_composer(app: &mut App, reason: String) {
    let Some(pane) = app.selected_pane() else {
        app.set_hint("no tmux pane on this row", HintLevel::Err);
        return;
    };
    let label = app.pane_label(&pane);
    app.collaboration_mailbox.open = false;
    app.collaboration_composer = Some(CollaborationComposer::new(
        CollaborationComposeTarget::Prompt { pane },
        label.clone(),
    ));
    // Lead with what *works* — this composer sends, to exactly the pane the
    // user pointed at. A hint that opens with "only —" plus a warning color
    // reads as a refusal, and users reported it as "m doesn't work" while
    // the composer sat there fully functional.
    app.set_hint(
        format!("▷ typing to {label} as keystrokes ({reason})"),
        HintLevel::Ok,
    );
}

/// The one room peer the selected row contains, if it contains exactly
/// one.
///
/// Lets `m` work at session granularity: the user points at the session
/// their peer is in without first expanding it to the exact pane. Two
/// peers in the same row stays ambiguous — picking one for the user
/// would silently address the wrong agent.
fn peer_inside_selected_row<'a>(app: &'a App, room: &'a RoomContext) -> Option<&'a Participant> {
    let row = app.selected_row()?;
    let mut inside = room
        .peers
        .iter()
        .filter(|peer| row.contains_pane(&peer.pane));
    let first = inside.next()?;
    inside.next().is_none().then_some(first)
}

/// Name the room, so "no peer here" points somewhere.
///
/// The room is the window `muxa watch` was opened from and is fixed for
/// the lifetime of the process. Selecting a different row cannot change
/// it, and the message has to say so — otherwise the obvious reading of
/// "here" is "the row I am looking at".
fn empty_room_hint(current: &Participant) -> String {
    let where_ = match (&current.tmux_session_name, &current.window_name) {
        (Some(session), Some(window)) => format!("{session}:{window}"),
        (Some(session), None) => session.clone(),
        _ => current.room.window_id.clone(),
    };
    format!(
        "no peer in {where_} — the room is the window watch was opened from, not the selected row; start another agent there"
    )
}

/// Name the rows that can actually receive a request.
///
/// Kept short enough to survive the single-line hint area: past three
/// peers the list is trimmed and the remainder counted, because a hint
/// that wraps off-screen helps nobody. `muxa peers` prints the full set.
fn peer_choice_hint(labels: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut names = labels
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if labels.len() > SHOWN {
        use std::fmt::Write as _;
        let _ = write!(names, " +{} more", labels.len() - SHOWN);
    }
    format!("select one of these rows, then press m: {names}")
}

fn open_watch_collaboration_composer(app: &mut App) {
    let Some(room) = app.collaboration.room.as_ref() else {
        let message = app
            .collaboration
            .unavailable
            .clone()
            .unwrap_or_else(|| collaboration_open_hint().into());
        open_prompt_only_composer(app, message);
        return;
    };
    if room.peers.is_empty() {
        // "here" is the window `muxa watch` was launched from, fixed at
        // startup — not the row under the cursor. The table spans every
        // session on the host, so a user looking at a row from a session
        // that *does* have two agents reads "no peer here" as plainly
        // false and moves the cursor, which changes nothing. Name the
        // room and say the cursor is not the lever.
        let reason = empty_room_hint(&room.current);
        open_prompt_only_composer(app, reason);
        return;
    }
    let selected_pane = app.selected_pane();
    let peer = selected_pane
        .as_deref()
        .and_then(|pane| app.collaboration.peer_for_pane(pane))
        // A session row is a whole tmux session, and the pane it resolves
        // to is whichever of its agents moved last. Requiring that drifting
        // pane to be the peer made `m` fail on a row that plainly contains
        // one — the user is pointing at the right session and being told to
        // point at it. Accept the row when exactly one peer lives in it;
        // more than one is genuinely ambiguous and still asks.
        .or_else(|| peer_inside_selected_row(app, room))
        .or_else(|| (room.peers.len() == 1).then(|| &room.peers[0]))
        // Take what the composer needs by value: everything below mutates
        // `app`, and holding a borrow into `app.collaboration` across that
        // is what the borrow checker is for.
        .map(|peer| (peer.pane.clone(), peer.label()));
    let Some((peer_pane, peer_label)) = peer else {
        // The table lists every tracked agent on the host — dozens of them
        // — while only the handful in this window can receive a request.
        // "choose an agent in this tmux window" is true and useless: it
        // does not say which rows those are, and nothing on screen marks
        // them. Name them instead.
        let labels = room
            .peers
            .iter()
            .map(|p| format!("{} · {}", p.label(), app.pane_label(&p.pane)))
            .collect::<Vec<_>>();
        let hint = peer_choice_hint(&labels);
        open_prompt_only_composer(app, hint);
        return;
    };
    let Some(origin) = app.collaboration.origin.clone() else {
        app.set_hint(collaboration_open_hint(), HintLevel::Err);
        return;
    };
    app.collaboration_mailbox.open = false;
    // `codex@%469` says who; the pane position says where to look for
    // them on screen. Both, because pane ids are stable and meaningless
    // while positions are legible and drift.
    let label = format!("{peer_label} · {}", app.pane_label(&peer_pane));
    app.collaboration_composer = Some(CollaborationComposer::new(
        CollaborationComposeTarget::Send {
            origin,
            target: format!("pane:{peer_pane}"),
            pane: peer_pane,
            kind: RequestKind::Question,
            mode: ComposeSendMode::ReadOnly,
        },
        label,
    ));
}

async fn run_watch_collaboration_composer(
    client: &Client,
    composer: CollaborationComposer,
) -> ActionOutcome {
    match composer.target {
        CollaborationComposeTarget::Send {
            origin,
            target,
            kind,
            mode,
            ..
        } => {
            let work_mode = match mode {
                ComposeSendMode::ReadOnly => WorkMode::ReadOnly,
                ComposeSendMode::Execute => WorkMode::Execute,
                // Enter converts just-send into a Quick(SendPrompt) before
                // anything reaches this runner; refuse loudly rather than
                // invent a contract for keystrokes.
                ComposeSendMode::JustSend => {
                    return ActionOutcome::Err(
                        "just-send types into the pane and cannot become a request".into(),
                    )
                }
            };
            let request = NewRequest {
                kind,
                body: composer.input,
                expects_reply: kind != RequestKind::Notice,
                work_mode,
                paths: Vec::new(),
                air_artifacts: Vec::new(),
            };
            match client.collaboration_send(&origin, &target, &request).await {
                Ok(request) => ActionOutcome::Ok(format!(
                    "sent {} to {} ({})",
                    short_collaboration_request_id(&request.id),
                    request.to.label(),
                    request_kind_label(kind)
                )),
                Err(error) => ActionOutcome::Err(format!("collaboration send failed: {error}")),
            }
        }
        CollaborationComposeTarget::Prompt { .. } => ActionOutcome::Err(
            "keystrokes go to the pane directly and cannot become a request".into(),
        ),
        CollaborationComposeTarget::Reply {
            origin,
            request_id,
            status,
        } => match client
            .collaboration_reply(&origin, &request_id, status, &composer.input, &[], &[])
            .await
        {
            Ok(request) => ActionOutcome::Ok(format!(
                "replied to {} ({})",
                short_collaboration_request_id(&request.id),
                request_status_label(status)
            )),
            Err(error) => ActionOutcome::Err(format!("collaboration reply failed: {error}")),
        },
    }
}

fn collaboration_requests(app: &App) -> &[CollaborationRequest] {
    match app.collaboration_mailbox.tab {
        CollaborationMailboxTab::Incoming => &app.collaboration.incoming,
        CollaborationMailboxTab::Sent => &app.collaboration.sent,
    }
}

fn selected_collaboration_request(app: &App) -> Option<&CollaborationRequest> {
    collaboration_requests(app).get(app.collaboration_mailbox.selected)
}

fn clamp_collaboration_mailbox(app: &mut App) {
    let len = collaboration_requests(app).len();
    app.collaboration_mailbox.selected = if len == 0 {
        0
    } else {
        app.collaboration_mailbox.selected.min(len - 1)
    };
}

fn move_collaboration_mailbox(app: &mut App, delta: isize) {
    let len = collaboration_requests(app).len();
    if len == 0 {
        app.collaboration_mailbox.selected = 0;
        return;
    }
    app.collaboration_mailbox.selected = app
        .collaboration_mailbox
        .selected
        .saturating_add_signed(delta)
        .min(len - 1);
}

fn toggle_collaboration_mailbox(app: &mut App) {
    app.collaboration_mailbox.tab = match app.collaboration_mailbox.tab {
        CollaborationMailboxTab::Incoming => CollaborationMailboxTab::Sent,
        CollaborationMailboxTab::Sent => CollaborationMailboxTab::Incoming,
    };
    app.collaboration_mailbox.selected = 0;
}

fn request_kind_label(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Question => "question",
        RequestKind::Review => "review",
        RequestKind::Task => "task",
        RequestKind::Notice => "notice",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollaborationBadge {
    icon: &'static str,
    label: &'static str,
    foreground: Color,
    background: Color,
}

impl CollaborationBadge {
    fn span(self) -> Span<'static> {
        Span::styled(
            format!(" {} {} ", self.icon, self.label),
            Style::default()
                .fg(self.foreground)
                .bg(self.background)
                .add_modifier(Modifier::BOLD),
        )
    }
}

fn request_kind_badge(kind: RequestKind) -> CollaborationBadge {
    match kind {
        RequestKind::Question => CollaborationBadge {
            icon: "?",
            label: "QUESTION",
            foreground: Color::Black,
            background: Color::Cyan,
        },
        RequestKind::Review => CollaborationBadge {
            icon: "◆",
            label: "REVIEW",
            foreground: Color::White,
            background: Color::Magenta,
        },
        RequestKind::Task => CollaborationBadge {
            icon: "▶",
            label: "TASK",
            foreground: Color::Black,
            background: Color::Yellow,
        },
        RequestKind::Notice => CollaborationBadge {
            icon: "!",
            label: "NOTICE",
            foreground: Color::White,
            background: Color::Blue,
        },
    }
}

fn work_mode_badge(mode: WorkMode) -> CollaborationBadge {
    match mode {
        WorkMode::ReadOnly => CollaborationBadge {
            icon: "○",
            label: "READ-ONLY",
            foreground: Color::Black,
            background: Color::Green,
        },
        WorkMode::Execute => CollaborationBadge {
            icon: "●",
            label: "EXECUTE",
            foreground: Color::White,
            background: Color::Red,
        },
    }
}

fn reply_status_badge(status: RequestStatus) -> CollaborationBadge {
    match status {
        RequestStatus::Completed => CollaborationBadge {
            icon: "✓",
            label: "COMPLETED",
            foreground: Color::Black,
            background: Color::Green,
        },
        RequestStatus::Blocked => CollaborationBadge {
            icon: "!",
            label: "BLOCKED",
            foreground: Color::Black,
            background: Color::Yellow,
        },
        RequestStatus::Declined => CollaborationBadge {
            icon: "×",
            label: "DECLINED",
            foreground: Color::White,
            background: Color::DarkGray,
        },
        RequestStatus::Failed => CollaborationBadge {
            icon: "■",
            label: "FAILED",
            foreground: Color::White,
            background: Color::Red,
        },
        _ => CollaborationBadge {
            icon: "·",
            label: request_status_label(status),
            foreground: Color::Black,
            background: Color::Gray,
        },
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

fn short_collaboration_request_id(request_id: &str) -> String {
    request_id.chars().take(18).collect()
}

async fn append_human_interaction(
    path: Option<&Path>,
    kind: HumanInteractionKind,
    app: &App,
    pane_id: Option<&str>,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
) {
    let Some(path) = path else {
        return;
    };
    if ended_at <= started_at {
        return;
    }
    let (session_id, session_name) = interaction_session(app, pane_id);
    let entry =
        ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
            kind,
            pane: pane_id.map(str::to_string),
            session_id,
            session_name,
            started_at,
            ended_at,
        }));
    if let Err(e) = muxa::activity::append_entry(path, &entry).await {
        tracing::warn!(error = %e, path = %path.display(), "could not append human interaction");
    }
}

fn interaction_session(app: &App, pane_id: Option<&str>) -> (Option<String>, Option<String>) {
    let session_name = pane_id.and_then(|id| {
        app.panes
            .iter()
            .find(|pane| pane.pane_id == id)
            .map(|pane| pane.session.clone())
    });
    let session_id = session_name.as_deref().and_then(|name| {
        app.sessions
            .iter()
            .find(|session| session.name == name)
            .map(|session| session.session_id.clone())
    });
    (session_id, session_name)
}

/// Pull every event already sitting in the OS-side terminal input
/// buffer without ever blocking. `poll(Duration::ZERO)` returns
/// immediately; we keep reading until nothing is left. Safe to call
/// from an async context since neither call yields.
fn drain_pending_events() -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    while crossterm::event::poll(Duration::ZERO)? {
        events.push(crossterm::event::read()?);
    }
    Ok(events)
}

#[derive(Debug)]
pub(crate) enum Action {
    None,
    Quit,
    Refresh,
    /// Attach to a pane that was pinned by an overlay.
    AttachPane(String),
    /// Pop open the preview overlay for the selected row.
    OpenPreview,
    /// Close the preview overlay and return to the table.
    ClosePreview,
    /// Swap the preview between popup and full-screen modes.
    TogglePreviewMode,
    /// Swap the preview content between prompt/response and live pane
    /// capture. Composes with `TogglePreviewMode` (geometry).
    TogglePreviewContent,
    /// Change the table's primary sort while staying inside the watch TUI.
    SetSort(WatchSortPreset),
    /// Change table granularity from the command palette.
    SetView(WatchView),
    /// Resolve the selected row as a same-window collaboration peer and open
    /// the durable request composer.
    OpenCollaborationMessage,
    /// Refresh and open incoming/sent collaboration history.
    OpenCollaborationMailbox,
    /// Submit the active collaboration request or reply composer.
    SubmitCollaboration,
    /// Close the active collaboration composer.
    CancelCollaborationComposer,
    /// Atomically claim the current agent's pending inbox.
    ClaimCollaborationInbox,
    /// Open a confirm popup for a destructive [`QuickAction`]. The
    /// popup itself is interpreted by the input loop; the action only
    /// dispatches when the user answers `y`.
    AskConfirm(ConfirmPopup),
    /// User answered `y` to the active confirm popup — dispatch the
    /// payload and clear the popup.
    ConfirmYes,
    /// User answered anything else (`n`, Esc, `q`, Tab, arrow keys).
    /// Just clears the popup with no side-effect.
    ConfirmCancel,
    /// Run a non-destructive quick action immediately (no confirm).
    /// Currently used for `c` (copy prompt) and the `?` help toggle.
    Quick(QuickAction),
    /// Surface a one-line "not applicable" hint in the footer because
    /// the requested action doesn't fit the selected row. String is
    /// the rendered hint body.
    NotApplicable(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct CommandSpec {
    command: &'static str,
    description: &'static str,
}

const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        command: "refresh",
        description: "refresh data now",
    },
    CommandSpec {
        command: "preview",
        description: "preview selected pane",
    },
    CommandSpec {
        command: "copy",
        description: "copy selected prompt",
    },
    CommandSpec {
        command: "message",
        description: "message selected room peer",
    },
    CommandSpec {
        command: "mailbox",
        description: "open collaboration mailbox",
    },
    CommandSpec {
        command: "attention",
        description: "toggle attention-only rows",
    },
    CommandSpec {
        command: "events",
        description: "open transition inbox",
    },
    CommandSpec {
        command: "inspector",
        description: "toggle wide-screen inspector",
    },
    CommandSpec {
        command: "sort latest",
        description: "sort by latest activity",
    },
    CommandSpec {
        command: "sort duration",
        description: "sort by session duration",
    },
    CommandSpec {
        command: "sort session",
        description: "sort by session name",
    },
    CommandSpec {
        command: "sort state",
        description: "sort by attention state",
    },
    CommandSpec {
        command: "view session",
        description: "group by session",
    },
    CommandSpec {
        command: "view pane",
        description: "show individual panes",
    },
    CommandSpec {
        command: "view swarm",
        description: "show swarm clusters",
    },
    CommandSpec {
        command: "kill",
        description: "kill selected pane (confirm)",
    },
    CommandSpec {
        command: "abort",
        description: "abort selected turn (confirm)",
    },
    CommandSpec {
        command: "help",
        description: "show keybindings",
    },
    CommandSpec {
        command: "quit",
        description: "exit muxa watch",
    },
];

fn command_suggestions(input: &str) -> Vec<CommandSpec> {
    let query = input.trim().to_lowercase();
    COMMAND_SPECS
        .iter()
        .copied()
        .filter(|spec| query.is_empty() || spec.command.starts_with(&query))
        .take(8)
        .collect()
}

fn execute_palette_command(app: &mut App, input: &str) -> Action {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let command = normalized.to_lowercase();
    match command.as_str() {
        "q" | "quit" => Action::Quit,
        "r" | "refresh" => Action::Refresh,
        "o" | "open" | "preview" => Action::OpenPreview,
        "m" | "message" => Action::OpenCollaborationMessage,
        "b" | "mailbox" => Action::OpenCollaborationMailbox,
        "copy" | "yank" => quick_copy_action(app),
        "kill" => quick_kill_action(app),
        "abort" => quick_abort_action(app),
        "help" | "?" => Action::Quick(QuickAction::ShowHelp),
        "attention" => {
            app.toggle_attention_only();
            app.set_hint(
                if app.attention_only {
                    "attention filter enabled"
                } else {
                    "attention filter disabled"
                },
                HintLevel::Ok,
            );
            Action::None
        }
        "events" => {
            app.toggle_event_inbox();
            Action::None
        }
        "inspector" => {
            app.toggle_inspector();
            Action::None
        }
        "sort latest" => Action::SetSort(WatchSortPreset::Latest),
        "sort duration" => Action::SetSort(WatchSortPreset::Duration),
        "sort session" => Action::SetSort(WatchSortPreset::Session),
        "sort state" | "sort attention" => Action::SetSort(WatchSortPreset::State),
        "view session" | "view sessions" => Action::SetView(WatchView::Session),
        "view pane" | "view panes" => Action::SetView(WatchView::Pane),
        "view swarm" => Action::SetView(WatchView::Swarm),
        "" => {
            app.set_hint("command: type a command or press Esc", HintLevel::Warn);
            Action::None
        }
        _ => {
            app.set_hint(format!("unknown command: {normalized}"), HintLevel::Warn);
            Action::None
        }
    }
}

#[allow(clippy::too_many_lines)] // one ordered input-mode dispatcher keeps modal precedence explicit
fn handle_event(ev: Event, app: &mut App) -> Action {
    // Bracketed paste arrives as one event carrying the whole payload.
    // Prompt/command modes keep it literal; table mode treats it as a search
    // query, matching ordinary direct typing.
    if let Event::Paste(pasted) = ev {
        if let Some(composer) = app.collaboration_composer.as_mut() {
            for c in pasted.chars() {
                composer.insert(c);
            }
        } else if let Some(command) = app.command_palette.as_mut() {
            command.insert_str(&pasted.replace(['\r', '\n'], " "));
        } else if app.preview.is_none()
            && !app.help_open
            && !app.event_inbox_open
            && !app.collaboration_mailbox.open
        {
            app.edit_search(|query| query.push_str(&pasted.replace(['\r', '\n'], " ")));
        }
        return Action::None;
    }

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

    // Ctrl-C is global — quits regardless of mode.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        return Action::Quit;
    }

    // Confirm popup steals every keystroke until resolved. Keeping the
    // accept gate to a single character (`y` / `Y` / Enter) and routing
    // everything else to `ConfirmCancel` is the deliberate "deliberately
    // type yes" safety rail — see `ConfirmPopup` doc.
    if app.confirm.is_some() {
        return match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Action::ConfirmYes,
            // Spec calls out: anything else (incl. n, Esc, q, Tab,
            // arrows) cancels. Listing them explicitly would invite
            // someone to accidentally drop one — fall through.
            _ => Action::ConfirmCancel,
        };
    }

    if app.collaboration_composer.is_some() {
        return handle_collaboration_composer_event(code, modifiers, app);
    }

    if app.command_palette.is_some() {
        return handle_command_event(code, modifiers, app);
    }

    // Help overlay: `?` toggles it; `q` / `Esc` close it. Anything
    // else passes through but is ignored — we don't want `c` while
    // the overlay is open to silently copy a prompt the user can't
    // see.
    if app.help_open {
        return match code {
            KeyCode::F(1) | KeyCode::Esc | KeyCode::Char('q' | '?') => {
                Action::Quick(QuickAction::ShowHelp)
            }
            _ => Action::None,
        };
    }

    if app.collaboration_mailbox.open {
        return handle_collaboration_mailbox_event(code, app);
    }

    if app.event_inbox_open {
        return match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.toggle_event_inbox();
                Action::None
            }
            KeyCode::Char(c)
                if modifiers.contains(KeyModifiers::ALT) && c.eq_ignore_ascii_case(&'e') =>
            {
                app.toggle_event_inbox();
                Action::None
            }
            _ => Action::None,
        };
    }

    if app.preview.is_some() {
        return handle_preview_event(code, app);
    }

    if modifiers.contains(KeyModifiers::ALT) {
        return match code {
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'p') => Action::OpenPreview,
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'r') => Action::Refresh,
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'s') => {
                Action::SetSort(WatchSortPreset::Session)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'l') => {
                Action::SetSort(WatchSortPreset::Latest)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'d') => {
                Action::SetSort(WatchSortPreset::Duration)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'t') => {
                Action::SetSort(WatchSortPreset::State)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'c') => quick_copy_action(app),
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'k') => quick_kill_action(app),
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'x') => quick_abort_action(app),
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'a') => {
                app.toggle_attention_only();
                Action::None
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'i') => {
                app.toggle_inspector();
                Action::None
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'e') => {
                app.toggle_event_inbox();
                Action::None
            }
            KeyCode::Char('?') => Action::Quick(QuickAction::ShowHelp),
            _ => Action::None,
        };
    }

    if app.pending_g && !matches!(code, KeyCode::Char('g') | KeyCode::Esc) {
        app.pending_g = false;
    }

    match code {
        KeyCode::F(1) => Action::Quick(QuickAction::ShowHelp),
        KeyCode::Esc if app.explicit_search || !app.search_query.is_empty() => {
            app.clear_search();
            Action::None
        }
        KeyCode::Esc if app.pending_g => {
            app.pending_g = false;
            Action::None
        }
        KeyCode::Esc if app.attention_only => {
            app.toggle_attention_only();
            Action::None
        }
        KeyCode::Esc => Action::Quit,
        KeyCode::Enter => quick_prompt_action(app),
        KeyCode::Down => {
            app.move_down();
            Action::None
        }
        KeyCode::Up => {
            app.move_up();
            Action::None
        }
        KeyCode::Right => {
            app.move_into_session();
            Action::None
        }
        KeyCode::Left => {
            app.move_to_session_parent();
            Action::None
        }
        KeyCode::Home => {
            app.move_first();
            Action::None
        }
        KeyCode::End => {
            app.move_last();
            Action::None
        }
        KeyCode::PageDown => {
            app.move_page_down();
            Action::None
        }
        KeyCode::PageUp => {
            app.move_page_up();
            Action::None
        }
        KeyCode::Char('u')
            if modifiers.contains(KeyModifiers::CONTROL)
                && (app.explicit_search || !app.search_query.is_empty()) =>
        {
            app.clear_search();
            Action::None
        }
        KeyCode::Char('w')
            if modifiers.contains(KeyModifiers::CONTROL)
                && (app.explicit_search || !app.search_query.is_empty()) =>
        {
            app.delete_search_word();
            Action::None
        }
        KeyCode::Char('u')
            if modifiers.contains(KeyModifiers::CONTROL) && app.browse_keys_active() =>
        {
            app.move_half_page_up();
            Action::None
        }
        KeyCode::Char('d')
            if modifiers.contains(KeyModifiers::CONTROL) && app.browse_keys_active() =>
        {
            app.move_half_page_down();
            Action::None
        }
        KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => Action::Refresh,
        KeyCode::Char('/') if app.browse_keys_active() => {
            app.arm_explicit_search();
            Action::None
        }
        KeyCode::Char(':') if app.browse_keys_active() => {
            app.command_palette = Some(CommandPalette::default());
            app.pending_g = false;
            Action::None
        }
        KeyCode::Char('q') if app.browse_keys_active() => Action::Quit,
        KeyCode::Char('?') if app.browse_keys_active() => Action::Quick(QuickAction::ShowHelp),
        KeyCode::Char('r') if app.browse_keys_active() => Action::Refresh,
        KeyCode::Char('o') if app.browse_keys_active() => Action::OpenPreview,
        KeyCode::Char('m') if app.browse_keys_active() => Action::OpenCollaborationMessage,
        KeyCode::Char('b') if app.browse_keys_active() => Action::OpenCollaborationMailbox,
        KeyCode::Char('h') if app.browse_keys_active() => {
            app.move_to_session_parent();
            Action::None
        }
        KeyCode::Char('l') if app.browse_keys_active() => {
            app.move_into_session();
            Action::None
        }
        KeyCode::Char('G') if app.browse_keys_active() => {
            app.move_last();
            Action::None
        }
        KeyCode::Char('g') if app.browse_keys_active() && app.pending_g => {
            app.pending_g = false;
            app.move_first();
            Action::None
        }
        KeyCode::Char('g') if app.browse_keys_active() => {
            app.pending_g = true;
            Action::None
        }
        KeyCode::Backspace => {
            app.edit_search(|query| {
                query.pop();
            });
            Action::None
        }
        KeyCode::Char('j')
            if app.browse_keys_active() && !modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.move_down();
            Action::None
        }
        KeyCode::Char('k')
            if app.browse_keys_active() && !modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.move_up();
            Action::None
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            app.edit_search(|query| query.push(c));
            Action::None
        }
        _ => Action::None,
    }
}

/// `:` command input owns keystrokes until Enter or Esc. Tab completes the
/// first visible suggestion; editing follows the same conventions as the
/// prompt composer and common shell command lines.
fn handle_command_event(code: KeyCode, modifiers: KeyModifiers, app: &mut App) -> Action {
    match code {
        KeyCode::Esc => {
            app.command_palette = None;
            Action::None
        }
        KeyCode::Enter => {
            let input = app
                .command_palette
                .take()
                .map(|command| command.input)
                .unwrap_or_default();
            execute_palette_command(app, &input)
        }
        KeyCode::Tab => {
            let completion = app
                .command_palette
                .as_ref()
                .and_then(|command| command_suggestions(&command.input).first().copied());
            if let (Some(command), Some(spec)) = (app.command_palette.as_mut(), completion) {
                command.input = spec.command.to_string();
                command.move_end();
            }
            Action::None
        }
        KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(command) = app.command_palette.as_mut() {
                command.delete_word();
            }
            Action::None
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(command) = app.command_palette.as_mut() {
                command.clear();
            }
            Action::None
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(command) = app.command_palette.as_mut() {
                command.insert(c);
            }
            Action::None
        }
        KeyCode::Backspace => {
            if let Some(command) = app.command_palette.as_mut() {
                command.backspace();
            }
            Action::None
        }
        KeyCode::Delete => {
            if let Some(command) = app.command_palette.as_mut() {
                command.delete();
            }
            Action::None
        }
        KeyCode::Left => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_left();
            }
            Action::None
        }
        KeyCode::Right => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_right();
            }
            Action::None
        }
        KeyCode::Home => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_home();
            }
            Action::None
        }
        KeyCode::End => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_end();
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// Resolve Tab inside the composer: cycle the per-target option, or say
/// why there is nothing to cycle.
fn composer_cycle_option(app: &mut App) {
    let Some(composer) = app.collaboration_composer.as_mut() else {
        return;
    };
    match &mut composer.target {
        // Kind is a request concept; in `just send` mode there is no
        // request, and silently cycling a hidden badge would surprise
        // whoever switches back.
        CollaborationComposeTarget::Send {
            mode: ComposeSendMode::JustSend,
            ..
        } => {
            app.set_hint(
                "kind applies to requests — Ctrl-E to leave just-send",
                HintLevel::Warn,
            );
        }
        CollaborationComposeTarget::Send { kind, .. } => {
            *kind = match *kind {
                RequestKind::Question => RequestKind::Review,
                RequestKind::Review => RequestKind::Task,
                RequestKind::Task => RequestKind::Notice,
                RequestKind::Notice => RequestKind::Question,
            };
        }
        CollaborationComposeTarget::Reply { status, .. } => {
            *status = match *status {
                RequestStatus::Completed => RequestStatus::Blocked,
                RequestStatus::Blocked => RequestStatus::Declined,
                RequestStatus::Declined => RequestStatus::Failed,
                _ => RequestStatus::Completed,
            };
        }
        CollaborationComposeTarget::Prompt { .. } => {
            app.set_hint(
                "requests need a same-window peer — keystrokes only here",
                HintLevel::Warn,
            );
        }
    }
}

/// Resolve Enter inside the composer.
///
/// `just send` bypasses the mailbox entirely: the text goes into the pane
/// as keystrokes, through the same Quick dispatch the old prompt popup
/// used. The composer is taken here (rather than in a run-loop arm) so
/// submit stays a single code path per mode.
fn composer_submit_action(app: &mut App) -> Action {
    let empty = app
        .collaboration_composer
        .as_ref()
        .is_none_or(|composer| composer.input.trim().is_empty());
    if empty {
        app.set_hint("message cannot be empty", HintLevel::Warn);
        return Action::None;
    }
    match app.collaboration_composer.take() {
        Some(CollaborationComposer {
            target:
                CollaborationComposeTarget::Send {
                    pane,
                    mode: ComposeSendMode::JustSend,
                    ..
                }
                | CollaborationComposeTarget::Prompt { pane },
            input,
            ..
        }) => Action::Quick(QuickAction::SendPrompt {
            pane_id: pane,
            text: input,
        }),
        other => {
            app.collaboration_composer = other;
            Action::SubmitCollaboration
        }
    }
}

fn handle_collaboration_composer_event(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Action {
    match code {
        KeyCode::Esc => Action::CancelCollaborationComposer,
        KeyCode::Enter => composer_submit_action(app),
        KeyCode::Tab => {
            composer_cycle_option(app);
            Action::None
        }
        KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
            match app.collaboration_composer.as_mut().map(|c| &mut c.target) {
                Some(CollaborationComposeTarget::Send { mode, .. }) => {
                    *mode = match *mode {
                        ComposeSendMode::ReadOnly => ComposeSendMode::Execute,
                        ComposeSendMode::Execute => ComposeSendMode::JustSend,
                        ComposeSendMode::JustSend => ComposeSendMode::ReadOnly,
                    };
                }
                Some(CollaborationComposeTarget::Prompt { .. }) => {
                    app.set_hint(
                        "requests need a same-window peer — keystrokes only here",
                        HintLevel::Warn,
                    );
                }
                _ => {}
            }
            Action::None
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.insert(c);
            }
            Action::None
        }
        KeyCode::Backspace => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.backspace();
            }
            Action::None
        }
        KeyCode::Delete => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.delete();
            }
            Action::None
        }
        KeyCode::Left => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_left();
            }
            Action::None
        }
        KeyCode::Right => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_right();
            }
            Action::None
        }
        KeyCode::Home => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_home();
            }
            Action::None
        }
        KeyCode::End => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_end();
            }
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_collaboration_mailbox_event(code: KeyCode, app: &mut App) -> Action {
    match code {
        KeyCode::Esc | KeyCode::Char('q' | 'b') => {
            app.collaboration_mailbox.open = false;
            Action::None
        }
        KeyCode::Tab | KeyCode::BackTab => {
            toggle_collaboration_mailbox(app);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_collaboration_mailbox(app, 1);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_collaboration_mailbox(app, -1);
            Action::None
        }
        KeyCode::Char('i') => Action::ClaimCollaborationInbox,
        KeyCode::Char('e') => {
            open_watch_collaboration_reply_composer(app);
            Action::None
        }
        KeyCode::Char('r') => Action::OpenCollaborationMailbox,
        _ => Action::None,
    }
}

fn open_watch_collaboration_reply_composer(app: &mut App) {
    if app.collaboration_mailbox.tab != CollaborationMailboxTab::Incoming {
        app.set_hint("switch to incoming requests to reply", HintLevel::Err);
        return;
    }
    let Some(origin) = app.collaboration.origin.clone() else {
        app.set_hint(collaboration_open_hint(), HintLevel::Err);
        return;
    };
    let Some(request) = selected_collaboration_request(app) else {
        app.set_hint("no incoming request selected", HintLevel::Err);
        return;
    };
    if request.status == RequestStatus::Queued {
        app.set_hint(
            "press i to claim the request before replying",
            HintLevel::Err,
        );
        return;
    }
    if request.status.is_terminal() {
        app.set_hint("selected request is already terminal", HintLevel::Err);
        return;
    }
    let request_id = request.id.clone();
    let label = format!(
        "{} · {}",
        request.from.label(),
        short_collaboration_request_id(&request_id)
    );
    app.collaboration_composer = Some(CollaborationComposer::new(
        CollaborationComposeTarget::Reply {
            origin,
            request_id,
            status: RequestStatus::Completed,
        },
        label,
    ));
}

fn preview_targets_for_pane(app: &App, pane_id: &str) -> Vec<String> {
    let Some(row) = app.rows.iter().find(|row| row.contains_pane(pane_id)) else {
        return Vec::new();
    };
    match row {
        WatchRow::Agent(a) => a.pane.clone().into_iter().collect(),
        WatchRow::BarePane(p) => vec![p.pane_id.clone()],
        WatchRow::Session(s) => session_preview_targets(s),
    }
}

fn session_preview_targets(s: &SessionRow) -> Vec<String> {
    let agent_panes: HashSet<String> = s.agents.iter().filter_map(|a| a.pane.clone()).collect();
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    for pane in &s.pane_ids {
        if agent_panes.contains(pane) && seen.insert(pane.clone()) {
            targets.push(pane.clone());
        }
    }
    for agent in &s.agents {
        let Some(pane) = agent.pane.as_ref() else {
            continue;
        };
        if seen.insert(pane.clone()) {
            targets.push(pane.clone());
        }
    }
    if targets.is_empty() {
        if let Some(pane) = s.representative_pane.as_ref() {
            targets.push(pane.clone());
        }
    }

    targets
}

fn preview_target_position(app: &App, pane_id: &str) -> Option<(usize, usize)> {
    let targets = preview_targets_for_pane(app, pane_id);
    let idx = targets.iter().position(|target| target == pane_id)?;
    Some((idx + 1, targets.len()))
}

fn cycle_preview_agent(app: &mut App, delta: isize) {
    let Some(current) = app.preview.as_ref().map(|p| p.pane_id.clone()) else {
        return;
    };
    let targets = preview_targets_for_pane(app, &current);
    if targets.len() <= 1 {
        return;
    }
    let Some(current_idx) = targets.iter().position(|pane| pane == &current) else {
        return;
    };
    let next_idx = if delta >= 0 {
        (current_idx + 1) % targets.len()
    } else if current_idx == 0 {
        targets.len() - 1
    } else {
        current_idx - 1
    };
    let next_pane = targets[next_idx].clone();

    let mut changed = false;
    if let Some(preview) = app.preview.as_mut() {
        if preview.pane_id != next_pane {
            preview.pane_id = next_pane;
            preview.scroll = 0;
            changed = true;
        }
    }
    if changed {
        app.pane_capture = None;
    }
}

/// Preview mode scrolls the overlay instead of the table and opens the
/// prompt composer against the preview-pinned pane, not the table cursor.
fn handle_preview_event(code: KeyCode, app: &mut App) -> Action {
    if matches!(code, KeyCode::Enter) {
        let pane_id = app
            .preview
            .as_ref()
            .expect("preview present")
            .pane_id
            .clone();
        return Action::AttachPane(pane_id);
    }

    match code {
        KeyCode::Char('q' | 'p' | 'o') | KeyCode::Esc => Action::ClosePreview,
        KeyCode::Char('f') => Action::TogglePreviewMode,
        KeyCode::Char('c') => Action::TogglePreviewContent,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char(']') | KeyCode::Tab => {
            cycle_preview_agent(app, 1);
            Action::None
        }
        KeyCode::Char('[') | KeyCode::BackTab => {
            cycle_preview_agent(app, -1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_add(1);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_sub(1);
            Action::None
        }
        KeyCode::PageDown => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_add(10);
            Action::None
        }
        KeyCode::PageUp => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_sub(10);
            Action::None
        }
        KeyCode::Home => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = 0;
            Action::None
        }
        _ => Action::None,
    }
}

/// Resolve a `K` keystroke against the current selection. Yields a
/// confirm popup when the row qualifies (Agent with a tmux pane), a
/// "not applicable" hint otherwise. Pulled into its own helper so
/// the same logic can be unit-tested without the `handle_event`
/// keystroke matrix in the way.
pub(crate) fn quick_kill_action(app: &App) -> Action {
    if app
        .selected_target()
        .is_some_and(|target| target.agent_idx.is_some())
    {
        return match app.selected_agent().and_then(|agent| agent.pane.as_deref()) {
            Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                message: format!("Kill pane {}?", app.pane_label(pane_id)),
                on_confirm: QuickAction::KillPane(pane_id.to_string()),
            }),
            None => Action::NotApplicable("kill: no tmux pane on this row"),
        };
    }
    match app.selected_row() {
        Some(WatchRow::Agent(a)) => match a.pane.as_deref() {
            Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                message: format!("Kill pane {}?", app.pane_label(pane_id)),
                on_confirm: QuickAction::KillPane(pane_id.to_string()),
            }),
            // Agent with no pane (Claude SDK sub-process whose
            // ancestry walk failed). `K` would be a no-op — surface
            // why.
            None => Action::NotApplicable("kill: no tmux pane on this row"),
        },
        Some(WatchRow::BarePane(p)) => Action::AskConfirm(ConfirmPopup {
            message: format!(
                "Kill pane {}:{}.{}?",
                p.session, p.window_index, p.pane_index
            ),
            on_confirm: QuickAction::KillPane(p.pane_id.clone()),
        }),
        Some(WatchRow::Session(s)) => match s.representative_pane.as_deref() {
            Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                message: format!("Kill pane {}?", app.pane_label(pane_id)),
                on_confirm: QuickAction::KillPane(pane_id.to_string()),
            }),
            None => Action::NotApplicable("kill: no tmux pane on this row"),
        },
        None => Action::NotApplicable("kill: no row selected"),
    }
}

/// Resolve `R` (abort current turn). Same shape as `quick_kill_action`
/// but the destructive verb in the popup says "Abort" instead of "Kill".
pub(crate) fn quick_abort_action(app: &App) -> Action {
    match app.selected_row() {
        Some(WatchRow::Agent(_) | WatchRow::Session(_)) => {
            match app.selected_agent().and_then(|agent| agent.pane.as_deref()) {
                Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                    message: format!("Abort current turn in {}?", app.pane_label(pane_id)),
                    on_confirm: QuickAction::AbortTurn(pane_id.to_string()),
                }),
                None => Action::NotApplicable("abort: not a tracked agent"),
            }
        }
        // Bare panes have no agent state to abort — Ctrl-C would still
        // reach the foreground process, but it's no longer a "muxa
        // agent action" in any meaningful sense. Skip rather than
        // surprise.
        Some(WatchRow::BarePane(_)) => Action::NotApplicable("abort: not a tracked agent"),
        None => Action::NotApplicable("abort: no row selected"),
    }
}

/// Resolve table-mode Enter: attach to the selected pane, immediately.
///
/// This used to open an inline prompt first, with an empty second Enter
/// meaning "attach after all" — a two-step riddle for the most common
/// action in the TUI. Typing at an agent now lives in the `m` composer,
/// whose Ctrl-E `just send` mode does what the prompt popup did.
pub(crate) fn quick_prompt_action(app: &App) -> Action {
    match app.selected_pane() {
        Some(pane_id) => Action::AttachPane(pane_id),
        None => Action::NotApplicable("attach: no tmux pane on this row"),
    }
}

/// Resolve `c` (copy last prompt). Non-destructive, so no confirm —
/// dispatches straight to `Quick`. Vetoes when there's no
/// prompt to copy so the user gets a hint instead of a silent no-op.
pub(crate) fn quick_copy_action(app: &App) -> Action {
    match app.selected_last_prompt() {
        Some(p) if !p.is_empty() => Action::Quick(QuickAction::CopyPrompt(p.to_string())),
        Some(_) | None => Action::NotApplicable("copy: no prompt on this row"),
    }
}

// ---- rendering ------------------------------------------------------------

pub(crate) fn render(f: &mut Frame, app: &mut App) {
    // Advance the animation clock once per paint so the swarm view's dot
    // spinners cycle. Harmless for the table views (which ignore it).
    app.anim_frame = app.anim_frame.wrapping_add(1);
    app.sync_auto_expansion();
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
    match app.preview.as_ref().map(|p| p.mode) {
        Some(PreviewMode::Fullscreen) => {
            render_preview(f, chunks[1], app);
        }
        Some(PreviewMode::Popup) => {
            // Render the table behind so the user keeps a sense of
            // "where am I in the list" — then `Clear` the popup area
            // (wipes the cells under it so the popup paints clean) and
            // render the preview on top.
            render_body(f, chunks[1], app);
            let popup_area = centered_rect(80, 70, chunks[1]);
            f.render_widget(Clear, popup_area);
            render_preview(f, popup_area, app);
        }
        None => {
            render_body(f, chunks[1], app);
        }
    }
    // Overlays land on top of either the preview or the table —
    // `Clear` first so the popup body isn't visible-through-the-popup.
    // Help and confirm are mutually exclusive: opening confirm closes
    // help (handled by `handle_event`'s mode gates) so we render
    // whichever is active without worrying about z-order between them.
    if app.help_open {
        // The help body is the complete keybinding matrix. Size it by
        // the actual line count so new bindings don't silently clip the
        // final rows on common terminal heights.
        let popup_area = help_popup_rect(chunks[1]);
        f.render_widget(Clear, popup_area);
        render_help(f, popup_area, app);
    }
    if app.event_inbox_open {
        let popup_area = centered_rect(76, 72, chunks[1]);
        f.render_widget(Clear, popup_area);
        render_event_inbox(f, popup_area, app);
    }
    if app.collaboration_mailbox.open {
        let popup_area = centered_rect(88, 78, chunks[1]);
        f.render_widget(Clear, popup_area);
        render_collaboration_mailbox(f, popup_area, app);
    }
    if app.confirm.is_some() {
        // 50 × 30 % keeps the popup small enough that the table
        // behind stays scannable, but still leaves room for the
        // borders + message line + spacer + y/N hint line on a
        // typical 24-row terminal. Smaller (20 %) clips the hint
        // line on shorter screens.
        let popup_area = centered_rect(50, 30, chunks[1]);
        f.render_widget(Clear, popup_area);
        render_confirm(f, popup_area, app);
    }
    if app.collaboration_composer.is_some() {
        let popup_area = bottom_prompt_rect(chunks[1]);
        f.render_widget(Clear, popup_area);
        render_collaboration_composer(f, popup_area, app);
    }
    if app.command_palette.is_some() {
        let popup_area = command_popup_rect(chunks[1]);
        f.render_widget(Clear, popup_area);
        render_command_palette(f, popup_area, app);
    }
    render_footer(f, chunks[2], app);
}

fn command_popup_rect(r: Rect) -> Rect {
    let width = r.width.saturating_mul(70).saturating_div(100).max(48);
    centered_rect_by_size(width, r.height.min(12), r)
}

fn render_command_palette(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;

    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let command = app
        .command_palette
        .as_ref()
        .expect("render_command_palette without command state");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(Span::styled(
            " commands · Enter run · Tab complete · Esc cancel ",
            theme.action_badge(),
        ));
    let inner = block.inner(area);
    let visible_input =
        truncate_prompt_input(&command.input, inner.width.saturating_sub(2) as usize);
    let suggestions = command_suggestions(&command.input);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(": ", theme.accent_badge()),
            Span::raw(visible_input.text.clone()),
        ]),
        Line::from(""),
    ];
    if suggestions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching command",
            theme.dim_style().add_modifier(Modifier::ITALIC),
        )));
    } else {
        for (index, spec) in suggestions.iter().enumerate() {
            let command_style = if index == 0 {
                theme.selected_style()
            } else {
                theme.table_header_style()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<18}", spec.command), command_style),
                Span::styled(spec.description, theme.dim_style()),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);

    let cursor_visible = command.cursor.saturating_sub(visible_input.skipped_chars);
    let before_cursor: String = visible_input.text.chars().take(cursor_visible).collect();
    let before_cursor_width = u16::try_from(before_cursor.width()).unwrap_or(u16::MAX);
    let cursor_x = inner
        .x
        .saturating_add(2)
        .saturating_add(before_cursor_width);
    if cursor_x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((cursor_x, inner.y));
    }
}

/// Render the `?` help overlay — a centred popup with one line per
/// keybinding. Body comes from `help_overlay_text()` so the snapshot
/// test can pin the exact contents.
fn render_help(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(Span::styled(
            " help · F1/Esc to close ",
            theme.accent_badge(),
        ));
    let lines: Vec<Line> = help_overlay_text()
        .into_iter()
        .map(|s| {
            // Section headers are bare (no leading space); body lines
            // are indented with two spaces. Bolding the headers gives
            // the overlay a scannable shape without adding ad-hoc
            // styling per line.
            if s.is_empty() {
                Line::from("")
            } else if s.starts_with("  ") {
                Line::from(s)
            } else {
                Line::from(Span::styled(
                    s,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            }
        })
        .collect();
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_event_inbox(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(Span::styled(
            " Events · Alt-E/Esc to close ",
            theme.accent_badge(),
        ));
    let inner_height = usize::from(block.inner(area).height);
    let now = OffsetDateTime::now_utc();
    let lines = if app.events.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "No transitions yet.",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Completions, errors, and input requests stay here.",
                theme.dim_style(),
            )),
        ]
    } else {
        app.events
            .iter()
            .take(inner_height)
            .map(|event| {
                let (glyph, style) = match event.kind {
                    WatchEventKind::Done => ("✓", Style::default().fg(Color::Green)),
                    WatchEventKind::Attention => (
                        crate::state_icon(event.state),
                        theme.state_style(event.state),
                    ),
                    WatchEventKind::Error => ("■", Style::default().fg(Color::Red)),
                };
                Line::from(vec![
                    Span::styled(
                        format!("{:<4} ", relative_time(event.occurred_at, now)),
                        theme.dim_style(),
                    ),
                    Span::styled(format!("{glyph} "), style.add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{:<24}", truncate_chars(&event.label, 24)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(truncate_chars(
                        &event.summary,
                        usize::from(area.width.saturating_sub(38)),
                    )),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render the destructive-action confirm popup. Centred, two lines:
/// the question and the y/N hint. Default focus is implicitly "No"
/// because the input handler only accepts `y` / `Y` / Enter as yes —
/// any other key cancels.
fn render_confirm(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let popup = app
        .confirm
        .as_ref()
        .expect("render_confirm without confirm");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .border_type(theme.border_type)
        .title(Span::styled(
            " confirm ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let body = vec![
        Line::from(Span::raw(popup.message.clone())),
        Line::from(""),
        // [N] capitalised is the visual cue for "default focus is No"
        // — a convention borrowed from APT/dpkg prompts.
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "[y]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("es / "),
            Span::styled(
                "[N]",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("o "),
            Span::styled(
                "(Esc/Tab/anything else cancels)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]),
    ];
    let paragraph = Paragraph::new(body).block(block);
    f.render_widget(paragraph, area);
}

fn render_collaboration_composer(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;

    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let composer = app
        .collaboration_composer
        .as_ref()
        .expect("render_collaboration_composer without composer");
    let (title, border_color) = collaboration_composer_title(composer, theme);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .border_type(theme.border_type)
        .title(title);
    let inner = block.inner(area);
    let visible_input =
        truncate_prompt_input(&composer.input, inner.width.saturating_sub(2) as usize);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> "),
            Span::raw(visible_input.text.clone()),
        ]))
        .block(block),
        area,
    );

    let cursor_visible = composer.cursor.saturating_sub(visible_input.skipped_chars);
    let before_cursor: String = visible_input.text.chars().take(cursor_visible).collect();
    let cursor_x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(before_cursor.width()).unwrap_or(u16::MAX));
    if inner.height > 0 && cursor_x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((cursor_x, inner.y));
    }
}

fn collaboration_composer_title(
    composer: &CollaborationComposer,
    theme: WatchThemeSpec,
) -> (Line<'static>, Color) {
    match composer.target {
        // just-send drops the kind and mode badges: there is no request,
        // so showing a QUESTION badge over raw keystrokes would claim a
        // contract that does not exist. The peerless Prompt form is the
        // same thing with the contract modes locked out.
        CollaborationComposeTarget::Send {
            mode: ComposeSendMode::JustSend,
            ..
        }
        | CollaborationComposeTarget::Prompt { .. } => (
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    " ▷ SEND ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" keystrokes · no contract, no reply ", theme.dim_style()),
                Span::styled(
                    format!(" → {}  ", composer.label),
                    theme.table_header_style(),
                ),
                Span::styled(" Ctrl-E ", theme.key_badge()),
                Span::raw("mode "),
            ]),
            Color::Gray,
        ),
        CollaborationComposeTarget::Send { kind, mode, .. } => {
            let work_mode = match mode {
                ComposeSendMode::Execute => WorkMode::Execute,
                _ => WorkMode::ReadOnly,
            };
            let kind_badge = request_kind_badge(kind);
            let mode_badge = work_mode_badge(work_mode);
            let border = if work_mode == WorkMode::Execute {
                mode_badge.background
            } else {
                kind_badge.background
            };
            (
                Line::from(vec![
                    Span::raw(" "),
                    kind_badge.span(),
                    Span::raw(" "),
                    mode_badge.span(),
                    Span::styled(
                        format!(" → {}  ", composer.label),
                        theme.table_header_style(),
                    ),
                    Span::styled(" Tab ", theme.key_badge()),
                    Span::raw("kind  "),
                    Span::styled(" Ctrl-E ", theme.key_badge()),
                    Span::raw("mode "),
                ]),
                border,
            )
        }
        CollaborationComposeTarget::Reply { status, .. } => {
            let status_badge = reply_status_badge(status);
            (
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(" REPLY ", theme.action_badge()),
                    Span::raw(" "),
                    status_badge.span(),
                    Span::styled(
                        format!(" → {}  ", composer.label),
                        theme.table_header_style(),
                    ),
                    Span::styled(" Tab ", theme.key_badge()),
                    Span::raw("status "),
                ]),
                status_badge.background,
            )
        }
    }
}

fn render_collaboration_mailbox(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(collaboration_mailbox_title(app, theme));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.collaboration.room.is_none() {
        let message = app
            .collaboration
            .unavailable
            .clone()
            .unwrap_or_else(|| collaboration_open_hint().into());
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    message,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled("Esc/b closes", theme.dim_style())),
            ]),
            inner,
        );
        return;
    }

    let has_air = selected_collaboration_request(app).is_some_and(|request| {
        !request.air_artifacts.is_empty()
            || request
                .reply
                .as_ref()
                .is_some_and(|reply| !reply.air_artifacts.is_empty())
    });
    let detail_height = if has_air { 8 } else { 6 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
        .split(inner);
    let max_lines = usize::from(chunks[0].height).max(1);
    let width = usize::from(chunks[0].width).saturating_sub(2);
    let lines = collaboration_mailbox_request_lines(app, width, max_lines, theme);
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let detail_block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border_style())
        .title(Span::styled(" selected request ", theme.dim_style()));
    let detail_width = usize::from(detail_block.inner(chunks[1]).width).max(1);
    let detail = selected_collaboration_request(app).map_or_else(
        || vec![Line::from(Span::styled("-", theme.dim_style()))],
        |request| collaboration_request_detail_lines(request, detail_width, theme),
    );
    f.render_widget(Paragraph::new(detail).block(detail_block), chunks[1]);
}

fn collaboration_mailbox_title(app: &App, theme: WatchThemeSpec) -> Line<'static> {
    let tab = app.collaboration_mailbox.tab;
    Line::from(vec![
        Span::styled(" collaboration ", theme.accent_badge()),
        Span::raw(" "),
        Span::styled(
            format!(" incoming {} ", app.collaboration.incoming.len()),
            if tab == CollaborationMailboxTab::Incoming {
                theme.action_badge()
            } else {
                theme.dim_style()
            },
        ),
        Span::raw(" "),
        Span::styled(
            format!(" sent {} ", app.collaboration.sent.len()),
            if tab == CollaborationMailboxTab::Sent {
                theme.action_badge()
            } else {
                theme.dim_style()
            },
        ),
    ])
}

fn collaboration_mailbox_request_lines(
    app: &App,
    width: usize,
    max_lines: usize,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    let requests = collaboration_requests(app);
    let selected = app.collaboration_mailbox.selected;
    let tab = app.collaboration_mailbox.tab;
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
                CollaborationMailboxTab::Incoming => request.from.label(),
                CollaborationMailboxTab::Sent => request.to.label(),
            };
            let body = request
                .body
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let air_badge = request.air_artifacts.first();
            let air_width = air_badge.map_or(0, |reference| reference.profile.label().len() + 3);
            let text = truncate_chars(
                &format!(
                    "{} {:<9} {:<10} {:<14} {}",
                    short_collaboration_request_id(&request.id),
                    request_kind_label(request.kind),
                    request_status_label(request.status),
                    peer,
                    body
                ),
                width.saturating_sub(air_width),
            );
            let mut spans = vec![Span::styled(
                if focused { "> " } else { "  " },
                if focused {
                    Style::default().fg(theme.action)
                } else {
                    theme.dim_style()
                },
            )];
            if let Some(reference) = air_badge {
                spans.push(air_artifact_badge(reference));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                text,
                if focused {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ));
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            match tab {
                CollaborationMailboxTab::Incoming => "no incoming requests",
                CollaborationMailboxTab::Sent => "no sent requests",
            },
            theme.dim_style(),
        )));
    }
    lines
}

fn collaboration_request_detail_lines(
    request: &CollaborationRequest,
    width: usize,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    let body = request
        .body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![
        Line::from(truncate_chars(
            &format!(
                "{} · {} → {}",
                request.id,
                request.from.label(),
                request.to.label()
            ),
            width,
        )),
        Line::from(truncate_chars(
            &format!(
                "{} · {} · {}",
                request_kind_label(request.kind),
                request_status_label(request.status),
                work_mode_label(request.work_mode)
            ),
            width,
        )),
    ];
    lines.extend(
        request
            .air_artifacts
            .iter()
            .map(|reference| air_artifact_detail_line("input", reference, width)),
    );
    lines.push(Line::from(truncate_chars(&format!("body: {body}"), width)));
    if let Some(reply) = request.reply.as_ref() {
        let reply_body = reply.body.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(Line::from(Span::styled(
            truncate_chars(
                &format!(
                    "reply [{}]: {reply_body}",
                    request_status_label(reply.status)
                ),
                width,
            ),
            Style::default().fg(Color::Green),
        )));
        lines.extend(
            reply
                .air_artifacts
                .iter()
                .map(|reference| air_artifact_detail_line("output", reference, width)),
        );
    }
    lines.truncate(7);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("-", theme.dim_style())));
    }
    lines
}

fn air_artifact_badge(reference: &AirArtifactReference) -> Span<'static> {
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

fn air_artifact_detail_line(
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
        air_artifact_badge(reference),
        Span::raw(truncate_chars(
            &format!(" {direction} · {short_id} · {label} · {locator}"),
            width.saturating_sub(reference.profile.label().len() + 2),
        )),
    ])
}

/// Prompt input should feel like a command bar, not a modal dialog: it
/// hugs the bottom of the current content area and consumes only the
/// border plus one editable line.
fn bottom_prompt_rect(r: Rect) -> Rect {
    let height = r.height.min(3);
    Rect {
        x: r.x,
        y: r.y.saturating_add(r.height.saturating_sub(height)),
        width: r.width,
        height,
    }
}

struct VisiblePromptInput {
    text: String,
    skipped_chars: usize,
}

fn truncate_prompt_input(input: &str, max_width: usize) -> VisiblePromptInput {
    use unicode_width::UnicodeWidthStr;

    if input.width() <= max_width {
        return VisiblePromptInput {
            text: input.to_string(),
            skipped_chars: 0,
        };
    }
    let mut chars: Vec<char> = input.chars().collect();
    let mut skipped = 0;
    while !chars.is_empty() {
        let candidate: String = chars.iter().collect();
        if candidate.width() < max_width {
            return VisiblePromptInput {
                text: format!("…{candidate}"),
                skipped_chars: skipped,
            };
        }
        chars.remove(0);
        skipped += 1;
    }
    VisiblePromptInput {
        text: "…".into(),
        skipped_chars: skipped,
    }
}

/// Compute a centred sub-rect of `r` sized as `percent_x` × `percent_y`
/// of the parent. Standard ratatui popup helper — three-way layout
/// vertically picks the middle band, then horizontally on that band.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}

fn centered_rect_by_size(width: u16, height: u16, r: Rect) -> Rect {
    let width = width.min(r.width);
    let height = height.min(r.height);
    let x = r.x + r.width.saturating_sub(width) / 2;
    let y = r.y + r.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn help_popup_rect(r: Rect) -> Rect {
    let width = r.width.saturating_mul(60) / 100;
    let body_height = u16::try_from(help_overlay_text().len()).unwrap_or(u16::MAX);
    let height = body_height.saturating_add(2);
    centered_rect_by_size(width, height, r)
}

/// Full-screen detail view for the agent / pane the user pinned with `o` or
/// `Alt-P`.
///
/// Lays out as: title (pane label + kind/state) → bold "Last prompt"
/// section → bold "Last response" section → optional notification block.
/// The whole pane is scrollable via the `PreviewState.scroll` offset that
/// `handle_event` mutates when the user hits `↑/↓` / `j/k` / `PageUp` /
/// `PageDown` / `Home`.
///
/// Looks up the row by `pane_id` every frame so background refreshes that
/// re-sort the table can't bump us onto a different agent's content.
fn render_preview(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let preview = app
        .preview
        .as_ref()
        .expect("render_preview without preview");

    let mode_tag = match preview.content {
        PreviewContent::PromptResponse => "prompt",
        PreviewContent::LivePane => "live",
    };
    let target_tag = preview_target_position(app, &preview.pane_id)
        .filter(|(_, total)| *total > 1)
        .map(|(idx, total)| format!(" · {idx}/{total}"))
        .unwrap_or_default();
    let title = format!(
        " preview · {} · {}{} ",
        app.pane_label(&preview.pane_id),
        mode_tag,
        target_tag
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(Span::styled(title, theme.accent_badge()));

    // Live-capture mode: render the cached `tmux capture-pane -ep`
    // output through `ansi-to-tui` so the source pane's colors / bold /
    // dim styling are preserved. Wrapping is OFF because tmux already
    // wrapped at the source pane's width — turning it on a second time
    // breaks alignment of TUIs running inside the captured pane.
    if matches!(preview.content, PreviewContent::LivePane) {
        let body = build_pane_capture_body(app, &preview.pane_id);
        let paragraph = Paragraph::new(body)
            .block(block)
            .scroll((preview.scroll, 0));
        f.render_widget(paragraph, area);
        return;
    }

    let lines = build_preview_lines(app, &preview.pane_id);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((preview.scroll, 0));
    f.render_widget(paragraph, area);
}

/// Materialize the capture cache into ratatui `Text`. Errors degrade
/// to a one-line placeholder rather than blowing up the render — a
/// half-rendered capture is easier to debug than a panic deep in the
/// frame path.
fn build_pane_capture_body<'a>(app: &'a App, pane_id: &str) -> ratatui::text::Text<'a> {
    use ansi_to_tui::IntoText;

    let placeholder = |msg: &str| {
        ratatui::text::Text::from(ratatui::text::Line::from(Span::styled(
            msg.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        )))
    };

    let Some(cached) = app.pane_capture.as_ref() else {
        return placeholder("(capturing pane…)");
    };
    if cached.pane_id != pane_id {
        // Stale entry from a previous selection — the main loop will
        // re-fetch on the next iteration.
        return placeholder("(capturing pane…)");
    }
    if cached.text.is_empty() {
        return placeholder("(pane gone or capture failed)");
    }
    cached
        .text
        .as_bytes()
        .into_text()
        .unwrap_or_else(|_| placeholder("(could not parse pane content)"))
}

/// Compose the textual body of the preview pane. Pulled out so unit tests
/// can assert on the rendered structure without going through ratatui.
fn build_preview_lines<'a>(app: &'a App, pane_id: &str) -> Vec<Line<'a>> {
    let mut out: Vec<Line<'a>> = Vec::new();

    let agent = app.rows.iter().find_map(|r| match r {
        WatchRow::Agent(a) if a.pane.as_deref() == Some(pane_id) => Some(a.as_ref()),
        WatchRow::Session(s) => s
            .agents
            .iter()
            .find(|a| a.pane.as_deref() == Some(pane_id))
            .or_else(|| {
                if s.representative_pane.as_deref() == Some(pane_id) {
                    s.latest_agent.as_ref()
                } else {
                    None
                }
            }),
        WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
    });

    let pane_label = pane_display(Some(pane_id), &app.panes);
    if let Some(agent) = agent {
        // Header line — pane label + kind + state, all on one row.
        out.push(Line::from(vec![
            Span::styled("pane: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(pane_label, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("    "),
            Span::styled("kind: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(agent.kind.to_string()),
            Span::raw("    "),
            Span::styled("state: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(agent.state.to_string()),
        ]));
        out.push(Line::from(""));

        push_section(&mut out, "Last prompt", agent.last_prompt.as_deref());
        out.push(Line::from(""));
        push_section(&mut out, "Last response", agent.last_response.as_deref());

        if agent.last_notification.is_some() {
            out.push(Line::from(""));
            push_section(
                &mut out,
                "Last notification",
                agent.last_notification.as_deref(),
            );
        }
    } else {
        // Pane no longer in the row set — agent finished, pane closed,
        // or the user pressed `p` on a bare-pane row that has no agent
        // metadata to surface. Tell them rather than rendering empty.
        out.push(Line::from(Span::styled(
            format!("pane {pane_label}"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            "no agent record for this pane.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        )));
        out.push(Line::from(Span::styled(
            "(press o / q / Esc / p to return to the picker)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    out
}

/// Append a `<title>:` heading then either the body lines (one per
/// `\n`-separated line in the source) or a dim "—" placeholder when the
/// field is empty / missing.
fn push_section<'a>(out: &mut Vec<Line<'a>>, title: &str, body: Option<&'a str>) {
    out.push(Line::from(Span::styled(
        format!("{title}:"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    match body {
        Some(s) if !s.trim().is_empty() => {
            for line in s.lines() {
                out.push(Line::from(line.to_string()));
            }
        }
        _ => {
            out.push(Line::from(Span::styled(
                "—",
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }
}

fn app_agent_states(app: &App) -> Vec<AgentState> {
    let mut states = Vec::new();
    for row in &app.rows {
        match row {
            WatchRow::Agent(agent) => states.push(agent.state),
            WatchRow::Session(session) => states.extend(session.agent_states.values().copied()),
            WatchRow::BarePane(_) => {}
        }
    }
    states
}

fn header_state_summary_spans(
    states: Vec<AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<Span<'static>> {
    if states.len() <= 1 {
        Vec::new()
    } else {
        state_summary_spans(states, theme, spin)
    }
}

#[allow(clippy::too_many_lines)]
fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let agents = app
        .rows
        .iter()
        .filter(|r| match r {
            WatchRow::Agent(_) => true,
            WatchRow::Session(s) => s.latest_agent.is_some(),
            WatchRow::BarePane(_) => false,
        })
        .count();
    let bare = app.rows.len() - agents;
    let now = app.last_refresh;
    let clock = format!(
        "{:02}:{:02}:{:02} UTC",
        now.hour(),
        now.minute(),
        now.second()
    );
    let agent_states = app_agent_states(app);
    let agent_total = agent_states.len();
    let spin = Spinner {
        frame: app.anim_frame,
        enabled: app.watch_cfg.spinner && icons_unicode(),
    };
    let state_summary = header_state_summary_spans(agent_states, theme, spin);

    let mut spans = vec![
        Span::styled(theme.title, theme.accent_badge()),
        Span::raw("  "),
    ];

    if app.watch_cfg.view == WatchView::Session {
        spans.push(Span::styled(
            format!("{} session{}", app.rows.len(), plural(app.rows.len())),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
        if state_summary.is_empty() {
            spans.push(Span::styled(
                format!("{agent_total} agent{}", plural(agent_total)),
                theme.dim_style(),
            ));
        } else {
            spans.extend(state_summary);
        }
    } else if state_summary.is_empty() {
        spans.push(Span::styled(
            format!("{agents} agent{}", plural(agents)),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("+ {bare} pane{}", plural(bare)),
            theme.dim_style(),
        ));
    } else {
        spans.extend(state_summary);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("+ {bare} pane{}", plural(bare)),
            theme.dim_style(),
        ));
    }

    // Paneless waiters have no row and no attend target, so fold them into
    // the header's attention count — otherwise a detached/SDK-hosted agent
    // that goes WaitingInput is invisible in the primary loop.
    if app.paneless_attention > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("+{} paneless waiting", app.paneless_attention),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if app.unread_events > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("◆ {} new", app.unread_events),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(room) = app.collaboration.room.as_ref() {
        if room.unread > 0 || room.unread_replies > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("mail {}/{}", room.unread, room.unread_replies),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!("sort {}", sort_label(&app.watch_cfg.sort)),
        theme.dim_style(),
    ));
    spans.push(Span::raw("   "));
    spans.push(Span::styled(clock, theme.dim_style()));

    let title = Line::from(spans);
    let title = if app.refresh_pending {
        let mut spans = title.spans;
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "↻ refreshing…",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        ));
        Line::from(spans)
    } else {
        title
    };

    let status_line = if let Some(e) = app.last_error.as_ref() {
        // The NotConnected variant already reads as a complete sentence
        // ("daemon not reachable at … — is `muxad` running? …"), so a
        // `daemon error: ` prefix would just stutter. Other IO errors
        // benefit from the prefix to mark them as daemon-related.
        let text = if e.self_describing {
            e.message.clone()
        } else {
            format!("daemon error: {}", e.message)
        };
        Line::from(Span::styled(text, Style::default().fg(Color::Red)))
    } else if app.explicit_search || !app.search_query.is_empty() || app.attention_only {
        let visible = app.visible_targets().len();
        let mut parts = Vec::new();
        if app.explicit_search && app.search_query.is_empty() {
            parts.push("filter: ▏".to_string());
        } else if !app.search_query.is_empty() {
            parts.push(format!("filter: {}", truncate_chars(&app.search_query, 80)));
        }
        if app.attention_only {
            parts.push("attention only".to_string());
        }
        parts.push(format!("{visible} shown"));
        Line::from(Span::styled(
            parts.join("  ·  "),
            Style::default().fg(theme.accent),
        ))
    } else {
        Line::from(Span::styled(
            "j/k move  ·  type or / filter  ·  : commands  ·  ? help",
            theme.dim_style().add_modifier(Modifier::DIM),
        ))
    };

    let header = Paragraph::new(vec![title, status_line]).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme.border_style())
            .border_type(theme.border_type),
    );
    f.render_widget(header, area);
}

/// Dispatch the main body: the k9s-style swarm console for
/// [`WatchView::Swarm`], otherwise the classic table.
fn render_body(f: &mut Frame, area: Rect, app: &mut App) {
    let split_inspector = app.inspector_enabled
        && area.width >= 120
        && app.selected_pane().is_some()
        && app.preview.is_none()
        && !app.help_open
        && !app.event_inbox_open
        && !app.collaboration_mailbox.open;
    // The composer is deliberately absent from that list. It is a two-line
    // overlay, exactly like the prompt popup, which never hid the inspector
    // — so `Enter` kept the peer's state on screen while `m` blanked it,
    // for no reason a user could infer. The inspector is *most* wanted
    // while composing: it is where you check what the peer is doing before
    // asking it for something. The mailbox and help stay in the list
    // because those are full-height panels that need the width.
    app.inspector_visible = split_inspector;
    if split_inspector {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(area);
        render_primary_body(f, columns[0], app);
        render_inspector(f, columns[1], app);
        return;
    }
    render_primary_body(f, area, app);
}

fn render_primary_body(f: &mut Frame, area: Rect, app: &mut App) {
    app.table_page_rows = usize::from(area.height.saturating_sub(3).max(1));
    if app.watch_cfg.view == WatchView::Swarm {
        render_swarm(f, area, app);
    } else {
        render_table(f, area, app);
    }
}

fn render_inspector(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some(pane_id) = app.selected_pane() else {
        return;
    };
    let mut title = format!(" Inspector · {} ", app.pane_label(&pane_id));
    if let Some(agent) = app.selected_agent() {
        title = format!(
            " Inspector · {} · {} {} ",
            app.pane_label(&pane_id),
            state_age_label(agent.state),
            relative_time(agent.state_entered_at, OffsetDateTime::now_utc())
        );
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(Span::styled(title, theme.accent_badge()));

    let mut lines = Vec::new();
    if let Some(agent) = app.selected_agent() {
        lines.push(Line::from(vec![
            Span::styled("kind ", theme.dim_style()),
            Span::raw(agent.kind.to_string()),
            Span::raw("  "),
            Span::styled("model ", theme.dim_style()),
            Span::raw(agent.model.as_deref().unwrap_or("—").to_string()),
        ]));
        let summary = agent
            .last_notification
            .as_deref()
            .or(agent.last_prompt.as_deref())
            .unwrap_or("—")
            .replace('\n', " ");
        lines.push(Line::from(vec![
            Span::styled("latest ", theme.dim_style()),
            Span::raw(truncate_chars(
                &summary,
                usize::from(area.width.saturating_sub(10)),
            )),
        ]));
        lines.push(Line::from(Span::styled(
            "─".repeat(usize::from(area.width.saturating_sub(2))),
            theme.dim_style(),
        )));
    }
    lines.extend(build_pane_capture_body(app, &pane_id).lines);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// cli-spinners frames: `dots` for parent agents, `dots2` (denser) for
// subagents, and a half-circle set for starting.
const SWARM_DOTS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SWARM_DOTS2: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
const SWARM_START: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// Whether the active `[ui] icons` set can render the braille/half-circle
/// spinner. Under `icons = "ascii"` the spinner is suppressed so a terminal
/// that can't draw unicode doesn't get mojibake — it falls back to the static
/// ascii `state_icon`.
fn icons_unicode() -> bool {
    matches!(crate::icon_set(), IconSet::Unicode)
}

fn swarm_glyph(state: AgentState, frame: usize) -> &'static str {
    if !icons_unicode() {
        return crate::state_icon(state);
    }
    match state {
        AgentState::Working => SWARM_DOTS[frame % SWARM_DOTS.len()],
        AgentState::Starting => SWARM_START[frame % SWARM_START.len()],
        other => crate::state_icon(other),
    }
}

fn subagent_glyph(frame: usize, phase: usize) -> &'static str {
    if !icons_unicode() {
        return crate::state_icon(AgentState::Working);
    }
    SWARM_DOTS2[frame.wrapping_add(phase) % SWARM_DOTS2.len()]
}

/// Short, fixed vocabulary for an agent kind — `AgentKind`'s own
/// `Display` is the wire form (`claude_code`), which is too wide for a
/// swarm row or a `muxa peek` box header.
pub(crate) fn agent_kind_short(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::GeminiCli => "gemini",
        AgentKind::Opencode => "opencode",
        AgentKind::Task => "task",
        AgentKind::Unknown => "agent",
    }
}

fn swarm_state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Starting => "starting",
        AgentState::WaitingInput => "waiting·in",
        AgentState::WaitingChoice => "waiting·y/n",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
    }
}

/// `◇subagents ▸shells +other` load badge. Prefers the hook-tracked named
/// subagent count over the process-tree scan.
fn swarm_load(agent: &Agent) -> String {
    if !agent.subagents.is_empty() {
        return format!("◇{}", agent.subagents.len());
    }
    let w = &agent.workload;
    let mut parts = Vec::new();
    if w.subagent_count > 0 {
        parts.push(format!("◇{}", w.subagent_count));
    }
    if w.shell_count > 0 {
        parts.push(format!("▸{}", w.shell_count));
    }
    let other = w
        .process_count
        .saturating_sub(w.subagent_count)
        .saturating_sub(w.shell_count);
    if other > 0 {
        parts.push(format!("+{other}"));
    }
    parts.join(" ")
}

/// One cluster header line: `▐ session [n]  <state spinners>  n⬆ n⊂`.
fn swarm_cluster_header(
    sr: &SessionRow,
    theme: WatchThemeSpec,
    frame: usize,
    is_sel: bool,
) -> Line<'static> {
    let working = sr
        .agents
        .iter()
        .filter(|a| a.state == AgentState::Working)
        .count();
    let subs: usize = sr.agents.iter().map(|a| a.subagents.len()).sum();
    let mut hdr = vec![
        Span::styled(
            if is_sel { "▐ " } else { "  " },
            Style::default().fg(theme.accent),
        ),
        Span::styled(
            sr.session.clone(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  [{}]  ", sr.agents.len().max(sr.pane_count)),
            theme.dim_style(),
        ),
    ];
    for a in &sr.agents {
        hdr.push(Span::styled(
            format!("{} ", swarm_glyph(a.state, frame)),
            theme.state_style(a.state),
        ));
    }
    if working > 0 || subs > 0 {
        hdr.push(Span::styled(
            format!(" {working}⬆ {subs}⊂"),
            theme.dim_style(),
        ));
    }
    Line::from(hdr)
}

/// One agent row plus its indented subagent tree (`├─ / └─` with a dot
/// spinner per in-flight Task child).
fn swarm_agent_lines(
    agent: &Agent,
    theme: WatchThemeSpec,
    frame: usize,
    pulse: Option<PulseKind>,
) -> Vec<Line<'static>> {
    // A live transition pulse takes over the leading glyph; otherwise the
    // spinner/static state glyph.
    let (glyph, gstyle) = match pulse {
        Some(kind) => pulse_glyph_style(kind, theme, frame),
        None => (
            swarm_glyph(agent.state, frame).to_string(),
            theme.state_style(agent.state),
        ),
    };
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(format!("{glyph} "), gstyle),
        Span::styled(
            format!("{:<9}", agent_kind_short(agent.kind)),
            Style::default(),
        ),
        Span::styled(
            format!("{:<12}", swarm_state_label(agent.state)),
            theme.state_style(agent.state),
        ),
    ];
    let load = swarm_load(agent);
    if !load.is_empty() {
        spans.push(Span::styled(
            format!("{load:<7}"),
            Style::default().fg(theme.state_working),
        ));
    }
    if let Some(p) = agent.last_prompt.as_deref() {
        let snippet = p.replace('\n', " ");
        spans.push(Span::styled(
            truncate_chars(&snippet, 42),
            theme.dim_style(),
        ));
    }
    let mut out = vec![Line::from(spans)];

    let n = agent.subagents.len();
    for (j, s) in agent.subagents.iter().enumerate() {
        let conn = if j + 1 == n { "└─" } else { "├─" };
        let mut ss = vec![
            Span::raw("       "),
            Span::styled(format!("{conn} "), theme.dim_style()),
            Span::styled(
                format!("{} ", subagent_glyph(frame, j)),
                Style::default().fg(theme.state_working),
            ),
            Span::styled(
                format!("{:<16}", truncate_chars(&s.kind, 16)),
                Style::default(),
            ),
        ];
        if let Some(d) = s.description.as_deref() {
            ss.push(Span::styled(truncate_chars(d, 32), theme.dim_style()));
        }
        out.push(Line::from(ss));
    }
    out
}

/// The swarm console: one cluster per tmux session, animated dot spinners
/// for working/starting agents, and an indented subagent tree under each
/// agent. Selection (j/k) highlights the session cluster.
fn render_swarm(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let frame = app.anim_frame;
    let selected = app.table_state.selected();
    let now_pulse = std::time::Instant::now();
    let mut lines: Vec<Line> = Vec::new();
    let mut sel_line: Option<usize> = None;

    let visible_targets = app.visible_targets();
    for (i, target) in visible_targets.iter().enumerate() {
        let row = &app.rows[target.row_idx];
        let WatchRow::Session(sr) = row else {
            continue;
        };
        let is_sel = selected == Some(i);
        if is_sel {
            sel_line = Some(lines.len());
        }
        lines.push(swarm_cluster_header(sr, theme, frame, is_sel));
        for a in &sr.agents {
            let pulse = app.active_pulse(a.kind, &a.session_id, now_pulse);
            lines.extend(swarm_agent_lines(a, theme, frame, pulse));
        }
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no agents — waiting for a hook or discovery scan",
            theme.dim_style(),
        )));
    }

    // Keep the selected cluster on screen without a full scrollbar widget.
    let inner_h = usize::from(area.height.saturating_sub(2));
    let total = lines.len();
    let scroll_lines = if total <= inner_h {
        0
    } else {
        sel_line
            .unwrap_or(0)
            .saturating_sub(inner_h / 3)
            .min(total - inner_h)
    };
    let scroll = u16::try_from(scroll_lines).unwrap_or(u16::MAX);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(Span::styled(" Swarm ", theme.table_header_style()));
    let para = Paragraph::new(lines).block(block).scroll((scroll, 0));
    f.render_widget(para, area);
}

#[allow(clippy::too_many_lines)] // column resolution + per-row cell/badge/pulse
                                 // assembly reads better inline than split across helpers
fn render_table(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let visible_targets = app.visible_targets();

    // Empty grid reads as "muxa is broken" rather than "nothing is
    // running yet". Replace it with a centered hint that tells the user
    // what to do — and, when rows exist but are all hidden paneless
    // agents, says so instead of implying there's nothing at all.
    if visible_targets.is_empty() {
        render_empty_table(f, area, app, theme);
        return;
    }

    let header_cells = app.columns.iter().map(|c| {
        let header = if app.watch_cfg.view == WatchView::Session && matches!(c, WatchColumn::Pane) {
            "SESSION"
        } else if matches!(c, WatchColumn::Prompt) && app.watch_cfg.summary != WatchSummary::Prompt
        {
            // The column still falls back to the last prompt, but its
            // headline content is the agent's own recap/title — label it
            // for what it usually shows rather than its last resort.
            "SUMMARY"
        } else {
            c.header()
        };
        Cell::from(header).style(theme.table_header_style())
    });
    let header = Row::new(header_cells).height(1);

    let now = OffsetDateTime::now_utc();
    let selected = app.table_state.selected();
    let detail_host = detail_host_column(&app.columns);
    let status_host = status_host_column(&app.columns);
    let spin = Spinner {
        frame: app.anim_frame,
        enabled: app.watch_cfg.spinner && icons_unicode(),
    };
    // A one-shot done/error flash overrides the State cell for its window.
    // Resolve per-row pulses up front so the row closure only touches app
    // *fields* — calling an `&self` method inside it would capture all of
    // `app` and clash with the `&mut app.table_state` render below.
    let anim_frame = app.anim_frame;
    let state_col = app
        .columns
        .iter()
        .position(|c| matches!(c, WatchColumn::State | WatchColumn::StateAge));
    // Host badges: only when the row set spans >1 host (the cross-
    // multiplexer console) do we tag each row's SESSION/PANE cell, so a
    // single-host user sees no change. The Pane column is the natural
    // host-identifying slot in both the session and pane views.
    let pane_col = app
        .columns
        .iter()
        .position(|c| matches!(c, WatchColumn::Pane));
    let multi_host = rows_multi_host(&app.rows);
    let row_pulses = resolve_row_pulses(app);
    let target_pulses: Vec<Option<PulseKind>> = visible_targets
        .iter()
        .map(|target| {
            target
                .agent_idx
                .map_or(row_pulses[target.row_idx], |agent_idx| {
                    let WatchRow::Session(session) = &app.rows[target.row_idx] else {
                        return None;
                    };
                    let agent = session.agents.get(agent_idx)?;
                    app.active_pulse(agent.kind, &agent.session_id, std::time::Instant::now())
                })
        })
        .collect();
    let rows: Vec<Row> = app
        .visible_targets()
        .iter()
        .enumerate()
        .map(|(i, target)| {
            let r = &app.rows[target.row_idx];
            let child_agent = match (r, target.agent_idx) {
                (WatchRow::Session(session), Some(agent_idx)) => session.agents.get(agent_idx),
                _ => None,
            };
            let mut texts: Vec<Text> = if let Some(agent) = child_agent {
                app.columns
                    .iter()
                    .map(|c| {
                        c.agent_text(agent, now, &app.panes, theme, spin, app.watch_cfg.summary)
                    })
                    .collect()
            } else {
                match r {
                    WatchRow::Agent(a) => app
                        .columns
                        .iter()
                        .map(|c| {
                            c.agent_text(a, now, &app.panes, theme, spin, app.watch_cfg.summary)
                        })
                        .collect(),
                    WatchRow::BarePane(p) => app.columns.iter().map(|c| c.bare_text(p)).collect(),
                    WatchRow::Session(s) => app
                        .columns
                        .iter()
                        .map(|c| {
                            c.session_text(s, now, &app.panes, theme, spin, app.watch_cfg.summary)
                        })
                        .collect(),
                }
            };

            // Overlay a transition pulse on the State cell.
            if let (Some(sc), Some(kind)) = (state_col, target_pulses[i]) {
                texts[sc] = pulse_cell(kind, theme, anim_frame);
            }

            if let Some(pc) = pane_col {
                if target.agent_idx.is_some() {
                    prepend_tree_prefix(&mut texts[pc], "  └─ ", theme.dim_style());
                } else if matches!(r, WatchRow::Session(_)) {
                    // Parent rows use one stable, glyph-free gutter. Triangle
                    // markers are East-Asian-width ambiguous in some macOS
                    // terminal fonts and made single/multi-pane labels appear
                    // offset even when ratatui measured them as one cell.
                    prepend_tree_prefix(&mut texts[pc], "  ", theme.dim_style());
                }
            }

            // Tag the row with its host when the console spans multiple.
            if multi_host {
                if let (Some(pc), Some(host)) = (pane_col, row_host(r)) {
                    prepend_host_badge(&mut texts[pc], host);
                }
            }

            let mut expanded = false;
            if Some(i) == selected && app.watch_cfg.detail.enabled {
                // Child rows used to suppress the selected-row detail entirely.
                // Resolve against the exact selected agent instead, while parent
                // session rows continue to use their latest-agent fallback.
                let child_row = child_agent.cloned().map(WatchRow::agent);
                let detail_row = child_row.as_ref().unwrap_or(r);
                let configured_detail =
                    format_detail(&app.watch_cfg.detail.template, detail_row, &app.panes, now);
                let workload_detail =
                    row_workload_badge(detail_row).map(|workload| format!("tree {workload}"));

                if detail_host == status_host {
                    let combined = match (configured_detail, workload_detail) {
                        (Some(detail), Some(workload)) => Some(format!("{detail} · {workload}")),
                        (Some(detail), None) => Some(detail),
                        (None, Some(workload)) => Some(workload),
                        (None, None) => None,
                    };
                    if let (Some(host), Some(detail)) = (detail_host, combined) {
                        texts[host] = stack_detail(std::mem::take(&mut texts[host]), &detail);
                        expanded = true;
                    }
                } else {
                    if let (Some(host), Some(detail)) = (detail_host, configured_detail) {
                        texts[host] = stack_detail(std::mem::take(&mut texts[host]), &detail);
                        expanded = true;
                    }
                    if let (Some(host), Some(workload)) = (status_host, workload_detail) {
                        texts[host] = stack_detail(std::mem::take(&mut texts[host]), &workload);
                        expanded = true;
                    }
                }
            }

            let row = Row::new(texts.into_iter().map(Cell::from).collect::<Vec<_>>());
            if expanded {
                row.height(2)
            } else {
                row
            }
        })
        .collect();

    let widths: Vec<Constraint> = app
        .columns
        .iter()
        .map(|c| resolve_width(*c, &app.watch_cfg))
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style())
                .border_type(theme.border_type)
                .title(if app.watch_cfg.view == WatchView::Session {
                    " Sessions "
                } else {
                    " Agents "
                }),
        )
        .row_highlight_style(theme.selected_style())
        // Keep the selection marker useful without shifting any columns: the
        // same two-cell gutter is reserved for every row and the header.
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);

    f.render_stateful_widget(table, area, &mut app.table_state);
}

/// Render the "no rows" placeholder inside the usual bordered table block.
/// Centered vertically and horizontally so it reads as a deliberate empty
/// state, not a rendering glitch. Distinguishes "nothing tracked at all"
/// from "everything visible is hidden as paneless" so the hint is
/// actionable in both cases.
fn render_empty_table(f: &mut Frame, area: Rect, app: &App, theme: WatchThemeSpec) {
    let title = if app.watch_cfg.view == WatchView::Session {
        " Sessions "
    } else {
        " Agents "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if !app.rows.is_empty() && (!app.search_query.is_empty() || app.attention_only) {
        let description = match (app.search_query.is_empty(), app.attention_only) {
            (false, true) => format!(
                "No attention items match ‘{}’.",
                truncate_chars(&app.search_query, 60)
            ),
            (false, false) => format!(
                "No sessions or agents match ‘{}’.",
                truncate_chars(&app.search_query, 60)
            ),
            (true, true) => "No agents currently need attention.".to_string(),
            (true, false) => unreachable!(),
        };
        lines.push(Line::from(Span::styled(
            description,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Backspace edits the filter; Esc clears it.",
            theme.dim_style(),
        )));
    } else if app.paneless_attention > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{} agent{} waiting — but with no tmux pane to show.",
                app.paneless_attention,
                plural(app.paneless_attention)
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Run `muxa watch --include-paneless` to list them.",
            theme.dim_style(),
        )));
    } else if app.paneless_hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "No panes to show — {} paneless agent{} hidden.",
                app.paneless_hidden,
                plural(app.paneless_hidden)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Run `muxa watch --include-paneless` to list them.",
            theme.dim_style(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "No agents tracked.",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Start an agent, or run `muxa doctor` to check setup.",
            theme.dim_style(),
        )));
    }

    // Pad with blank lines above so the message sits vertically centered
    // in the block's inner area.
    let text_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let top_pad = inner.height.saturating_sub(text_lines) / 2;
    let mut padded: Vec<Line> = Vec::with_capacity((top_pad as usize) + lines.len());
    for _ in 0..top_pad {
        padded.push(Line::from(""));
    }
    padded.extend(lines);

    let paragraph = Paragraph::new(padded).alignment(Alignment::Center);
    f.render_widget(paragraph, inner);
}

/// Pick which column hosts the expanded detail line. Prefer `Prompt` (the
/// natural fit and what the default template targets); otherwise the
/// last column, which is usually the widest catch-all (`Activity`,
/// or any `Min`-constrained column the user added).
fn detail_host_column(cols: &[WatchColumn]) -> Option<usize> {
    if cols.is_empty() {
        return None;
    }
    cols.iter()
        .position(|c| matches!(c, WatchColumn::Prompt))
        .or(Some(cols.len() - 1))
}

fn status_host_column(cols: &[WatchColumn]) -> Option<usize> {
    if cols.is_empty() {
        return None;
    }
    cols.iter()
        .position(|c| matches!(c, WatchColumn::Pane))
        .or(Some(0))
}

/// Stack a dim "↳ detail" hint underneath `top` so the host cell renders
/// as 2 lines. Caller must also bump the row height to 2.
fn stack_detail<'a>(top: Text<'a>, detail: &str) -> Text<'a> {
    let mut lines: Vec<Line<'a>> = top.lines;
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!("↳ {}", truncate_chars(detail, 240)),
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::DIM | Modifier::ITALIC),
    )));
    Text::from(lines)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Build the detail string by interpolating `{name}` placeholders against
/// the row. Returns `None` when the resulting string is empty after
/// trimming (so callers can skip rendering an empty hint).
///
/// Newlines in source values are collapsed to ` · ` so the detail stays
/// on one visual line — the row is fixed at height 2.
fn format_detail(
    template: &str,
    row: &WatchRow,
    panes: &[PaneInfo],
    now: OffsetDateTime,
) -> Option<String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for nc in chars.by_ref() {
            if nc == '}' {
                closed = true;
                break;
            }
            name.push(nc);
        }
        if !closed {
            out.push('{');
            out.push_str(&name);
            continue;
        }
        // Unknown placeholder: leave the literal so the user sees it
        // and can fix the typo, rather than silently producing empty.
        //
        // Pipe-separated alternatives (`{a|b|c}`) resolve left-to-right
        // and pick the first variable that produces a non-placeholder
        // value — used to keep the detail row useful when the preferred
        // field hasn't been populated yet (e.g. `{last_response|last_prompt}`
        // gracefully falls back to the user's prompt while the agent is
        // still mid-turn or for older agents that pre-date transcript
        // tailing). If every alternative is empty/dash the literal is
        // left in place, mirroring the unknown-placeholder behaviour.
        if let Some(v) = resolve_var_chain(&name, row, panes, now) {
            out.push_str(&collapse_lines(&v));
        } else {
            out.push('{');
            out.push_str(&name);
            out.push('}');
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() || trimmed == "—" || trimmed == "-" {
        None
    } else {
        Some(out)
    }
}

fn collapse_lines(s: &str) -> String {
    s.lines().collect::<Vec<_>>().join(" · ")
}

fn resolve_var(
    name: &str,
    row: &WatchRow,
    panes: &[PaneInfo],
    now: OffsetDateTime,
) -> Option<String> {
    match row {
        WatchRow::Agent(a) => Some(match name {
            "pane" => pane_display(a.pane.as_deref(), panes),
            "kind" => a.kind.to_string(),
            "state" => a.state.to_string(),
            "model" => a.model.clone().unwrap_or_else(|| "—".into()),
            "ctx" => a
                .context_used_pct
                .map_or_else(|| "—".into(), |p| format!("{p:.0}%")),
            "cost" => a
                .cost_usd
                .map_or_else(|| "—".into(), |c| format!("${c:.2}")),
            "activity" => relative_time(a.last_activity_at, now),
            "last_prompt" => a.last_prompt.clone().unwrap_or_else(|| "—".into()),
            "last_response" => a.last_response.clone().unwrap_or_else(|| "—".into()),
            "last_notification" => a.last_notification.clone().unwrap_or_else(|| "—".into()),
            "cwd" => a.cwd.clone().unwrap_or_else(|| "—".into()),
            "workload" => workload_detail_string(a),
            "rate_limit" => limits_string(a, now),
            // `rate_limit_resets_at` prefers an active cap's reset time
            // (matches the badge the user sees), falling back to whichever
            // window has utilisation data so the placeholder stays useful
            // pre-cap.
            "rate_limit_resets_at" => a
                .rate_limited_until
                .or(a.rate_limit_5h_resets_at)
                .or(a.rate_limit_7d_resets_at)
                .map_or_else(
                    || "-".into(),
                    |t| {
                        t.format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_else(|_| t.to_string())
                    },
                ),
            "rate_limit_scope" => match a.rate_limit_scope {
                Some(RateLimitScope::FiveHour) => "5h".into(),
                Some(RateLimitScope::SevenDay) => "7d".into(),
                Some(RateLimitScope::Unknown) => "unknown".into(),
                None => "-".into(),
            },
            _ => return None,
        }),
        WatchRow::BarePane(p) => Some(match name {
            "pane" => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
            "kind" => p.current_command.clone(),
            "last_prompt" => {
                if p.title.is_empty() || p.title == p.current_command {
                    p.current_command.clone()
                } else {
                    p.title.clone()
                }
            }
            "state"
            | "model"
            | "ctx"
            | "cost"
            | "activity"
            | "last_response"
            | "last_notification"
            | "cwd"
            | "workload"
            | "rate_limit"
            | "rate_limit_resets_at"
            | "rate_limit_scope" => "—".into(),
            _ => return None,
        }),
        WatchRow::Session(s) => {
            if let Some(a) = s.latest_agent.as_ref() {
                return resolve_var(name, &WatchRow::agent(a.clone()), panes, now);
            }
            Some(match name {
                "pane" => s.session.clone(),
                "kind" => "session".into(),
                "last_prompt" => s.bare_summary.clone().unwrap_or_else(|| "—".into()),
                "activity" => s.activity.as_ref().map_or_else(
                    || "—".into(),
                    |a| format_duration(a.effective_total_secs(now)),
                ),
                "state"
                | "model"
                | "ctx"
                | "cost"
                | "last_response"
                | "last_notification"
                | "cwd"
                | "workload"
                | "rate_limit"
                | "rate_limit_resets_at"
                | "rate_limit_scope" => "—".into(),
                _ => return None,
            })
        }
    }
}

/// Resolve a placeholder spec that may contain pipe-separated alternatives
/// (e.g. `last_response|last_prompt`). Returns the first variable that
/// produces a non-empty, non-dash value. If every alternative is unknown
/// or empty, returns `None` so the caller leaves the literal placeholder
/// in place.
fn resolve_var_chain(
    spec: &str,
    row: &WatchRow,
    panes: &[PaneInfo],
    now: OffsetDateTime,
) -> Option<String> {
    let mut saw_known = false;
    for name in spec.split('|') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let Some(value) = resolve_var(name, row, panes, now) else {
            continue;
        };
        saw_known = true;
        let trimmed = value.trim();
        if !trimmed.is_empty() && trimmed != "—" && trimmed != "-" {
            return Some(value);
        }
    }
    // All alternatives produced placeholder values, but at least one was a
    // known variable. Surface the dash so the row still reads as "this
    // field is currently empty" rather than as a typo.
    if saw_known {
        Some("—".into())
    } else {
        None
    }
}

/// Text for the summary column under `mode`, collapsed to one line and
/// clipped to the column's practical width.
///
/// Each mode names the highest tier it will show and then *degrades*: the
/// default `Recap` falls through recap → session title → last prompt. That
/// matters because a recap is sparse (Claude Code writes one only when you
/// return after being away) and agents with no recap source at all — Codex,
/// Gemini — would otherwise render an empty column.
fn summary_line(a: &Agent, mode: WatchSummary) -> String {
    let picked = match mode {
        WatchSummary::Recap => a
            .recap
            .as_deref()
            .or(a.ai_title.as_deref())
            .or(a.last_prompt.as_deref()),
        WatchSummary::Title => a.ai_title.as_deref().or(a.last_prompt.as_deref()),
        WatchSummary::Prompt => a.last_prompt.as_deref(),
    };
    picked
        .unwrap_or("-")
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

fn workload_text(a: &Agent) -> Text<'static> {
    let label = agent_workload_badge(a).unwrap_or_else(|| "-".into());
    if label == "-" {
        return Text::from(Span::styled(
            "-",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    Text::from(Span::styled(label, Style::default().fg(Color::Cyan)))
}

fn session_workload_text(s: &SessionRow) -> Text<'static> {
    let label = session_workload_badge(s).unwrap_or_else(|| "-".into());
    if label == "-" {
        return Text::from(Span::styled(
            "-",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    Text::from(Span::styled(label, Style::default().fg(Color::Cyan)))
}

fn row_workload_badge(row: &WatchRow) -> Option<String> {
    match row {
        WatchRow::Agent(a) => agent_workload_badge(a),
        WatchRow::Session(s) => session_workload_badge(s),
        WatchRow::BarePane(_) => None,
    }
}

fn agent_workload_badge(a: &Agent) -> Option<String> {
    let mut parts = Vec::new();
    push_workload_parts(
        &mut parts,
        u32::from(a.workload.subagent_count),
        u32::from(a.workload.shell_count),
        workload_other_count(
            u32::from(a.workload.process_count),
            u32::from(a.workload.subagent_count),
            u32::from(a.workload.shell_count),
        ),
    );
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn session_workload_badge(s: &SessionRow) -> Option<String> {
    let (subagents, shells, other) = s.agents.iter().fold((0u32, 0u32, 0u32), |acc, a| {
        (
            acc.0 + u32::from(a.workload.subagent_count),
            acc.1 + u32::from(a.workload.shell_count),
            acc.2
                + workload_other_count(
                    u32::from(a.workload.process_count),
                    u32::from(a.workload.subagent_count),
                    u32::from(a.workload.shell_count),
                ),
        )
    });
    let mut parts = Vec::new();
    push_workload_parts(&mut parts, subagents, shells, other);
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn workload_other_count(process_count: u32, subagent_count: u32, shell_count: u32) -> u32 {
    process_count
        .saturating_sub(subagent_count)
        .saturating_sub(shell_count)
}

fn push_workload_parts(parts: &mut Vec<String>, subagent_count: u32, shell_count: u32, other: u32) {
    if subagent_count > 0 {
        parts.push(format!("◇{subagent_count}"));
    }
    if shell_count > 0 {
        parts.push(format!("▸{shell_count}"));
    }
    if other > 0 {
        parts.push(format!("+{other}"));
    }
}

fn workload_detail_string(a: &Agent) -> String {
    let w = &a.workload;
    if w.is_empty() {
        return "—".into();
    }
    let mut parts = Vec::new();
    if w.subagent_count > 0 {
        parts.push(format!("subagent:{}", w.subagent_count));
    }
    if w.shell_count > 0 {
        parts.push(format!("shell:{}", w.shell_count));
    }
    let other = w
        .process_count
        .saturating_sub(w.subagent_count)
        .saturating_sub(w.shell_count);
    if other > 0 {
        parts.push(format!("process:{other}"));
    }
    if w.helper_count > 0 {
        parts.push(format!("helper:{}", w.helper_count));
    }
    if !w.preview.is_empty() {
        let chain = w
            .preview
            .iter()
            .map(|p| match p.kind {
                WorkloadProcessKind::Shell => format!("{}(sh)", p.command),
                WorkloadProcessKind::Subagent => format!("{}(sub)", p.command),
                WorkloadProcessKind::Helper => format!("{}(helper)", p.command),
                WorkloadProcessKind::Process => p.command.clone(),
            })
            .collect::<Vec<_>>()
            .join(" -> ");
        parts.push(chain);
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" · ")
    }
}

/// True when the agent is *currently* rate-limited from the renderer's
/// point of view. Two sources can mark a row capped:
///
/// 1. `rate_limit_scope` set — every `RateLimited` event sets this,
///    regardless of whether a reset timestamp was on the wire. Cleared
///    on the next `Started`. This is the load-bearing signal: a
///    `StopFailure` 429 carries no `resets_at`, so without this gate
///    the user would see the row flip to `Error` but the LIMITS column
///    stay blank.
/// 2. `rate_limited_until` in the future — for sources that did carry
///    a reset time, treat the cap as auto-expired once that moment
///    passes (the daemon clears the field lazily on the next `Started`,
///    but in-flight snapshots can still carry a stale value).
///
/// Logic: capped iff scope is set AND (no reset known OR reset is in
/// the future).
fn is_currently_capped(a: &Agent, now: OffsetDateTime) -> bool {
    if a.rate_limit_scope.is_none() {
        return false;
    }
    a.rate_limited_until.is_none_or(|until| until > now)
}

/// Plain-string form of the LIMITS column payload — used both by the
/// styled cell renderer (`limits_text`) and the detail-line template's
/// `{rate_limit}` placeholder. Returns `"-"` when no rate-limit info is
/// known so the detail row reads like every other "no data" field.
fn limits_string(a: &Agent, now: OffsetDateTime) -> String {
    if is_currently_capped(a, now) {
        return format!(
            "⛔ {}",
            format_cap_body(a.rate_limit_scope, a.rate_limited_until, now)
        );
    }
    match (a.rate_limit_5h_pct, a.rate_limit_7d_pct) {
        (Some(five), Some(seven)) => {
            if seven > five {
                format!("7d {seven:.0}%")
            } else {
                format!("5h {five:.0}%")
            }
        }
        (Some(p), None) => format!("5h {p:.0}%"),
        (None, Some(p)) => format!("7d {p:.0}%"),
        (None, None) => "-".into(),
    }
}

/// Body of the cap badge, after the `⛔ ` glyph: `[scope-prefix] [reset]`.
/// Reset is rendered as a relative duration ("in 2h 14m") rather than a
/// wall-clock time — locale-free, no syscall, and unambiguous when the
/// daemon can't surface a local offset (multi-threaded tokio runtime
/// without the `time/local-offset` feature). When no reset time is
/// known (`StopFailure` 429), prints just the scope.
fn format_cap_body(
    scope: Option<RateLimitScope>,
    until: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> String {
    let scope_prefix = match scope {
        Some(RateLimitScope::FiveHour) => "5h",
        Some(RateLimitScope::SevenDay) => "7d",
        Some(RateLimitScope::Unknown) | None => "",
    };
    match until {
        Some(t) => {
            let suffix = format_relative_until(t, now);
            if scope_prefix.is_empty() {
                suffix
            } else {
                format!("{scope_prefix} {suffix}")
            }
        }
        None => {
            if scope_prefix.is_empty() {
                "rate limited".into()
            } else {
                format!("{scope_prefix} capped")
            }
        }
    }
}

/// Render the gap from `now` to `until` as a compact relative string.
/// Always positive: callers gate on `until > now` before invoking, but
/// the saturating arithmetic below guarantees safe output even if a
/// stale snapshot slips through.
///
/// Examples: `"in 2h 14m"`, `"in 47m"`, `"in 30s"`, `"now"`.
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

/// Build the styled `Text` for the LIMITS cell. Red when the agent has
/// actually been told it's capped; yellow when utilisation is ≥ 80%;
/// dim grey for the empty fallback so the column reads as "no data" at
/// a glance.
fn limits_text(a: &Agent, now: OffsetDateTime) -> Text<'static> {
    if is_currently_capped(a, now) {
        return Text::from(Span::styled(
            limits_string(a, now),
            Style::default().fg(Color::Red),
        ));
    }
    let max_pct = match (a.rate_limit_5h_pct, a.rate_limit_7d_pct) {
        (Some(p5), Some(p7)) => Some(p5.max(p7)),
        (Some(p), None) | (None, Some(p)) => Some(p),
        (None, None) => None,
    };
    match max_pct {
        Some(p) => {
            let style = if p >= 80.0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            Text::from(Span::styled(limits_string(a, now), style))
        }
        None => Text::from(Span::styled(
            "-",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )),
    }
}

fn relative_time(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let delta = now - at;
    let secs = delta.whole_seconds();
    if secs < 0 {
        // Clock skew — show absolute.
        return format!("{:02}:{:02}", at.hour(), at.minute());
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = delta.whole_minutes();
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = delta.whole_hours();
    if hours < 48 {
        return format!("{hours}h");
    }
    let days = delta.whole_days();
    format!("{days}d")
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    // Transient action hint takes priority over keybinding strips —
    // the user just pressed a key and wants to see the result. Falls
    // off after `FOOTER_HINT_TTL` so the keybinding strip comes back
    // automatically; we don't bother clearing the slot since the
    // freshness check is cheap.
    if let Some(hint) = app.footer_hint.as_ref() {
        if hint.fresh() {
            let style = match hint.level {
                HintLevel::Ok => Style::default().fg(Color::Green),
                HintLevel::Err => Style::default().fg(Color::Red),
                HintLevel::Warn => Style::default().fg(Color::Yellow),
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(hint.message.clone(), style))),
                area,
            );
            return;
        }
    }

    if render_contextual_footer(f, area, app, theme) {
        return;
    }

    let mut spans = Vec::new();
    // Put row-specific warnings first so they survive footer clipping on
    // narrow popup layouts.
    if selected_has_no_pane(app) {
        spans.push(Span::styled(
            "no tmux pane — attach unavailable",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
        spans.push(Span::raw("    "));
    }
    if app.paneless_hidden > 0 {
        spans.push(Span::styled(
            format!(
                "+{} paneless (use --include-paneless to show)",
                app.paneless_hidden
            ),
            theme
                .dim_style()
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
        spans.push(Span::raw("    "));
    }
    spans.extend([
        Span::styled(" j/k ", theme.key_badge()),
        Span::raw(" move  "),
        Span::styled(" h/l ", theme.key_badge()),
        Span::raw(" tree  "),
        Span::styled(" / ", theme.action_badge()),
        Span::raw(" filter  "),
        Span::styled(" : ", theme.action_badge()),
        Span::raw(" commands  "),
        Span::styled(" ⏎ ", theme.action_badge()),
        Span::raw(" prompt  "),
        Span::styled(" o ", theme.key_badge()),
        Span::raw(" preview  "),
        Span::styled(" m ", theme.action_badge()),
        Span::raw(" message  "),
        Span::styled(" b ", theme.key_badge()),
        Span::raw(" mailbox  "),
        Span::styled(" ? ", theme.key_badge()),
        Span::raw(" help"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_contextual_footer(f: &mut Frame, area: Rect, app: &App, theme: WatchThemeSpec) -> bool {
    if app.command_palette.is_some() {
        let spans = vec![
            Span::styled(" Enter ", theme.action_badge()),
            Span::raw(" run  "),
            Span::styled(" Tab ", theme.key_badge()),
            Span::raw(" complete  "),
            Span::styled(" Ctrl-W ", theme.key_badge()),
            Span::raw(" delete word  "),
            Span::styled(" Esc ", theme.key_badge()),
            Span::raw(" cancel"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.collaboration_composer.is_some() {
        render_collaboration_composer_footer(f, area, app, theme);
        return true;
    }

    if app.collaboration_mailbox.open {
        let spans = vec![
            Span::styled(" Tab ", theme.action_badge()),
            Span::raw("incoming/sent  "),
            Span::styled(" j/k ", theme.key_badge()),
            Span::raw("select  "),
            Span::styled(" i ", theme.action_badge()),
            Span::raw("claim  "),
            Span::styled(" e ", theme.action_badge()),
            Span::raw("reply  "),
            Span::styled(" Esc/b ", theme.key_badge()),
            Span::raw("close"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if let Some(preview) = app.preview.as_ref() {
        render_preview_footer(f, area, app, preview, theme);
        return true;
    }

    if app.explicit_search || !app.search_query.is_empty() {
        let spans = vec![
            Span::styled(" type ", theme.action_badge()),
            Span::raw(" filter  "),
            Span::styled(" Backspace ", theme.key_badge()),
            Span::raw(" edit  "),
            Span::styled(" Ctrl-W ", theme.key_badge()),
            Span::raw(" delete word  "),
            Span::styled(" Ctrl-U/Esc ", theme.key_badge()),
            Span::raw(" clear"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.pending_g {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" g… ", theme.action_badge()),
                Span::raw("press g for first row · Esc cancels"),
            ])),
            area,
        );
        return true;
    }
    false
}

fn render_collaboration_composer_footer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    theme: WatchThemeSpec,
) {
    let target = app
        .collaboration_composer
        .as_ref()
        .map(|composer| &composer.target);
    // The peerless prompt form advertises no Tab/Ctrl-E: both keys only
    // explain why they do nothing, and a footer that lists dead keys
    // teaches the user to stop reading footers.
    let mut spans = vec![
        Span::styled(" Enter ", theme.action_badge()),
        Span::raw("send  "),
    ];
    match target {
        Some(CollaborationComposeTarget::Send { .. }) => {
            spans.extend([
                Span::styled(" Tab ", theme.key_badge()),
                Span::raw("kind  "),
            ]);
        }
        Some(CollaborationComposeTarget::Reply { .. }) => {
            spans.extend([
                Span::styled(" Tab ", theme.key_badge()),
                Span::raw("status  "),
            ]);
        }
        Some(CollaborationComposeTarget::Prompt { .. }) | None => {}
    }
    if matches!(target, Some(CollaborationComposeTarget::Send { .. })) {
        spans.extend([
            Span::styled(" Ctrl-E ", theme.key_badge()),
            Span::raw("read-only/execute/just-send  "),
        ]);
    }
    spans.extend([
        Span::styled(" Esc ", theme.key_badge()),
        Span::raw("cancel"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_preview_footer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    preview: &PreviewState,
    theme: WatchThemeSpec,
) {
    // Preview mode rebinds the table-mode keybinds to their preview-pane
    // analogues. Toggle labels describe where the next keypress goes.
    let toggle_label = match preview.mode {
        PreviewMode::Popup => " fullscreen  ",
        PreviewMode::Fullscreen => " popup  ",
    };
    let content_label = match preview.content {
        PreviewContent::PromptResponse => " live pane  ",
        PreviewContent::LivePane => " prompt  ",
    };
    let mut spans = vec![
        Span::styled(" ↑/↓ ", theme.key_badge()),
        Span::raw(" scroll  "),
        Span::styled(" PgUp/PgDn ", theme.key_badge()),
        Span::raw(" page  "),
    ];
    if preview_target_position(app, &preview.pane_id).is_some_and(|(_, total)| total > 1) {
        spans.push(Span::styled(" [ / ] ", theme.key_badge()));
        spans.push(Span::raw(" agent  "));
    }
    spans.extend([
        Span::styled(" f ", theme.key_badge()),
        Span::raw(toggle_label),
        Span::styled(" c ", theme.key_badge()),
        Span::raw(content_label),
        Span::styled(" ⏎ ", theme.action_badge()),
        Span::raw(" prompt  "),
        Span::styled(" r ", theme.key_badge()),
        Span::raw(" refresh  "),
        Span::styled(" o/p/q/Esc ", theme.key_badge()),
        Span::raw(" back"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn selected_has_no_pane(app: &App) -> bool {
    app.selected_row().is_some() && app.selected_pane().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::collaboration::RoomId;
    use muxa::event::{AgentKind, AgentState};
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;
    use time::OffsetDateTime;

    #[test]
    fn state_markers_are_single_cell() {
        let theme = watch_theme(WatchTheme::Classic);
        for state in [
            AgentState::Working,
            AgentState::WaitingInput,
            AgentState::WaitingChoice,
            AgentState::Error,
            AgentState::Idle,
            AgentState::Starting,
            AgentState::Stopped,
        ] {
            let (marker, _) = state_marker(state, theme, Spinner::OFF);
            assert_eq!(unicode_width::UnicodeWidthStr::width(marker), 1);
        }
    }

    #[test]
    fn spinner_animates_only_working_and_starting() {
        let theme = watch_theme(WatchTheme::Classic);
        let on = Spinner {
            frame: 0,
            enabled: true,
        };
        // Working / starting animate to dot / half-circle frames.
        assert!(SWARM_DOTS.contains(&state_marker(AgentState::Working, theme, on).0));
        assert!(SWARM_START.contains(&state_marker(AgentState::Starting, theme, on).0));
        // Every other state keeps the shared static icon (also used by
        // `muxa status` / status-line).
        for state in [
            AgentState::Idle,
            AgentState::WaitingInput,
            AgentState::WaitingChoice,
            AgentState::Error,
            AgentState::Stopped,
        ] {
            assert_eq!(state_marker(state, theme, on).0, crate::state_icon(state));
        }
        // Advancing the frame advances the dot.
        let f0 = state_marker(
            AgentState::Working,
            theme,
            Spinner {
                frame: 0,
                enabled: true,
            },
        )
        .0;
        let f1 = state_marker(
            AgentState::Working,
            theme,
            Spinner {
                frame: 1,
                enabled: true,
            },
        )
        .0;
        assert_ne!(f0, f1);
        // Disabled falls back to the static icon everywhere.
        assert_eq!(
            state_marker(AgentState::Working, theme, Spinner::OFF).0,
            crate::state_icon(AgentState::Working)
        );
    }

    #[test]
    fn pulse_kind_classifies_transitions() {
        // Turn finished.
        assert_eq!(
            pulse_kind(Some(AgentState::Working), AgentState::Idle),
            Some(PulseKind::Done)
        );
        // Entered error from anywhere but error.
        assert_eq!(
            pulse_kind(Some(AgentState::WaitingInput), AgentState::Error),
            Some(PulseKind::Error)
        );
        // No flash: already error, first sight, spin-up settling, or non-idle.
        assert_eq!(pulse_kind(Some(AgentState::Error), AgentState::Error), None);
        assert_eq!(pulse_kind(None, AgentState::Error), None);
        assert_eq!(
            pulse_kind(Some(AgentState::Starting), AgentState::Idle),
            None
        );
        assert_eq!(
            pulse_kind(Some(AgentState::Working), AgentState::WaitingInput),
            None
        );
    }

    #[test]
    fn done_pulse_flashes_state_cell_on_work_to_idle() {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            hide_paneless: false,
            spinner: true,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let working = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("refactor"),
            None,
            None,
            None,
        );
        let panes = vec![fake_pane("%1", "a", 0, 0, "claude")];
        // Prime prev-state = Working (first sight never flashes).
        app.set_data(vec![working.clone()], panes.clone());
        app.detect_pulses();
        // Working → Idle arms a Done pulse.
        let mut idle = working;
        idle.state = AgentState::Idle;
        app.set_data(vec![idle], panes);
        app.detect_pulses();

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let screen = (0..terminal.backend().buffer().area().height)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains('✓'),
            "done pulse ✓ should overlay the State cell:\n{screen}"
        );
    }

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
            tmux_socket: None,
            tmux_session: None,
            kind,
            session_id: session.into(),
            surface: None,
            pane: pane.map(Into::into),
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            cwd: None,
            state,
            last_prompt: prompt.map(Into::into),
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: None,
            model: model.map(Into::into),
            context_used_pct: ctx,
            cost_usd: cost,
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
    fn workload_cell_summarizes_shells_and_processes() {
        let mut agent = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("prompt"),
            None,
            None,
            None,
        );
        agent.workload = muxa::WorkloadSummary {
            primary_pid: Some(20),
            process_count: 2,
            shell_count: 1,
            subagent_count: 0,
            helper_count: 1,
            preview: vec![
                muxa::WorkloadProcess {
                    pid: 30,
                    parent_pid: 20,
                    depth: 2,
                    kind: WorkloadProcessKind::Shell,
                    command: "zsh".into(),
                },
                muxa::WorkloadProcess {
                    pid: 31,
                    parent_pid: 30,
                    depth: 3,
                    kind: WorkloadProcessKind::Process,
                    command: "python3".into(),
                },
            ],
        };

        assert_eq!(agent_workload_badge(&agent).as_deref(), Some("▸1 +1"));
        assert_eq!(
            workload_detail_string(&agent),
            "shell:1 · process:1 · helper:1 · zsh(sh) -> python3"
        );
    }

    #[test]
    fn helper_only_workload_stays_out_of_primary_cell() {
        let mut agent = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("prompt"),
            None,
            None,
            None,
        );
        agent.workload = muxa::WorkloadSummary {
            primary_pid: Some(20),
            process_count: 0,
            shell_count: 0,
            subagent_count: 0,
            helper_count: 1,
            preview: Vec::new(),
        };

        assert_eq!(agent_workload_badge(&agent), None);
        assert_eq!(workload_detail_string(&agent), "helper:1");
    }

    fn fake_pane(pane: &str, session: &str, window: u32, pane_idx: u32, cmd: &str) -> PaneInfo {
        PaneInfo {
            socket: None,
            pane_id: pane.into(),
            session_id: String::new(),
            session: session.into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: window.to_string(),
            pane_index: pane_idx.to_string(),
            tty: "/dev/pts/0".into(),
            current_command: cmd.into(),
            title: cmd.into(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    fn fake_collaboration_participant(
        pane: &str,
        session_id: &str,
        alias: Option<&str>,
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
            roles: Vec::new(),
        }
    }

    fn fake_watch_collaboration_request(
        id: &str,
        from: Participant,
        to: Participant,
        status: RequestStatus,
    ) -> CollaborationRequest {
        let now = OffsetDateTime::now_utc();
        CollaborationRequest {
            id: id.into(),
            from,
            to,
            kind: RequestKind::Review,
            body: "review the auth change".into(),
            expects_reply: true,
            work_mode: WorkMode::ReadOnly,
            paths: Vec::new(),
            air_artifacts: Vec::new(),
            status,
            created_at: now,
            claimed_at: (status == RequestStatus::Claimed).then_some(now),
            notified_at: None,
            reply_notified_at: None,
            reply_read_at: None,
            reply: None,
        }
    }

    fn collaboration_watch_app() -> App {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            hide_paneless: false,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent(
                    "self",
                    Some("%1"),
                    AgentKind::Codex,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "peer",
                    Some("%2"),
                    AgentKind::Codex,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![
                fake_pane("%1", "main", 0, 0, "codex"),
                fake_pane("%2", "main", 0, 1, "codex"),
            ],
        );
        let current = fake_collaboration_participant("%1", "self", Some("builder"));
        let peer = fake_collaboration_participant("%2", "peer", Some("reviewer"));
        app.collaboration = WatchCollaboration {
            origin: Some(CollaborationOrigin {
                pane: "%1".into(),
                socket: Some("default".into()),
            }),
            room: Some(RoomContext {
                current,
                peers: vec![peer],
                unread: 0,
                unread_replies: 0,
            }),
            ..WatchCollaboration::default()
        };
        app
    }

    #[test]
    fn watch_collaboration_origin_uses_launch_pane_and_socket() {
        let origin = watch_collaboration_origin_from(
            Some("%9".into()),
            Some("/tmp/tmux-1000/custom,42,7".into()),
        )
        .unwrap();

        assert_eq!(origin.pane, "%9");
        assert_eq!(origin.socket.as_deref(), Some("custom"));
        assert!(watch_collaboration_origin_from(Some("zellij:3".into()), None).is_none());
    }

    #[test]
    fn watch_message_uses_the_only_room_peer_without_extra_target_steps() {
        let mut app = collaboration_watch_app();
        app.table_state.select(Some(0));

        open_watch_collaboration_composer(&mut app);

        assert!(matches!(
            app.collaboration_composer
                .as_ref()
                .map(|composer| &composer.target),
            Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%2"
        ));
    }

    #[test]
    fn watch_m_and_b_are_browse_actions() {
        let mut app = collaboration_watch_app();

        assert!(matches!(
            key_action(&mut app, 'm'),
            Action::OpenCollaborationMessage
        ));
        assert!(matches!(
            key_action(&mut app, 'b'),
            Action::OpenCollaborationMailbox
        ));
    }

    #[test]
    fn watch_collaboration_composer_cycles_contract_and_submits() {
        let mut app = collaboration_watch_app();
        open_watch_collaboration_composer(&mut app);

        assert!(matches!(
            handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app),
            Action::None
        ));
        assert!(matches!(
            handle_collaboration_composer_event(
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                &mut app
            ),
            Action::None
        ));
        let composer = app.collaboration_composer.as_ref().unwrap();
        assert!(matches!(
            composer.target,
            CollaborationComposeTarget::Send {
                kind: RequestKind::Review,
                mode: ComposeSendMode::Execute,
                ..
            }
        ));

        app.collaboration_composer.as_mut().unwrap().insert('검');
        assert!(matches!(
            handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
            Action::SubmitCollaboration
        ));
    }

    #[test]
    fn m_without_a_room_still_opens_a_keystrokes_composer() {
        // Collaboration being unavailable must not take `m` away — the
        // user is pointing at a pane and typing at it needs no contract.
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .expect("fixture has a pane-bearing row");
        app.table_state.select(Some(pane_idx));

        open_watch_collaboration_composer(&mut app);

        assert!(matches!(
            app.collaboration_composer.as_ref().map(|c| &c.target),
            Some(CollaborationComposeTarget::Prompt { pane }) if pane == "%42"
        ));

        app.collaboration_composer.as_mut().unwrap().insert('하');
        let action =
            handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
        assert!(
            matches!(
                action,
                Action::Quick(QuickAction::SendPrompt { ref pane_id, ref text })
                    if pane_id == "%42" && text == "하"
            ),
            "got {action:?}"
        );
    }

    #[test]
    fn the_prompt_only_composer_refuses_to_invent_a_contract() {
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .unwrap();
        app.table_state.select(Some(pane_idx));
        open_watch_collaboration_composer(&mut app);

        for key in [KeyCode::Tab, KeyCode::Char('e')] {
            let modifiers = if key == KeyCode::Char('e') {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            assert!(matches!(
                handle_collaboration_composer_event(key, modifiers, &mut app),
                Action::None
            ));
            assert!(
                matches!(
                    app.collaboration_composer.as_ref().map(|c| &c.target),
                    Some(CollaborationComposeTarget::Prompt { .. })
                ),
                "{key:?} must not conjure a request out of a peerless composer"
            );
        }
    }

    #[test]
    fn ctrl_e_cycles_through_just_send_and_back() {
        let mut app = collaboration_watch_app();
        open_watch_collaboration_composer(&mut app);
        let mode_of = |app: &App| match app.collaboration_composer.as_ref().unwrap().target {
            CollaborationComposeTarget::Send { mode, .. } => mode,
            _ => unreachable!(),
        };
        assert_eq!(mode_of(&app), ComposeSendMode::ReadOnly);
        for expected in [
            ComposeSendMode::Execute,
            ComposeSendMode::JustSend,
            ComposeSendMode::ReadOnly,
        ] {
            handle_collaboration_composer_event(
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                &mut app,
            );
            assert_eq!(mode_of(&app), expected);
        }
    }

    #[test]
    fn enter_in_just_send_mode_becomes_keystrokes_not_a_request() {
        let mut app = collaboration_watch_app();
        open_watch_collaboration_composer(&mut app);
        // ReadOnly -> Execute -> JustSend
        for _ in 0..2 {
            handle_collaboration_composer_event(
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                &mut app,
            );
        }
        app.collaboration_composer.as_mut().unwrap().insert('안');
        let action =
            handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
        assert!(
            matches!(
                action,
                Action::Quick(QuickAction::SendPrompt { ref pane_id, ref text })
                    if pane_id == "%2" && text == "안"
            ),
            "got {action:?}"
        );
        // The composer is consumed — nothing left to double-submit.
        assert!(app.collaboration_composer.is_none());
    }

    #[test]
    fn m_resolves_a_peer_from_the_session_row_that_holds_it() {
        // Pointing at the session the peer lives in is enough; the user
        // should not have to expand it to the exact pane first.
        let mut app = collaboration_watch_app();
        app.apply_view(WatchView::Session);
        app.table_state.select(Some(0));

        open_watch_collaboration_composer(&mut app);

        assert!(
            matches!(
                app.collaboration_composer
                    .as_ref()
                    .map(|composer| &composer.target),
                Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%2"
            ),
            "expected the peer inside the selected session row"
        );
    }

    #[test]
    fn the_empty_room_hint_names_the_window_and_denies_the_cursor() {
        // The table spans every session on the host, so an unqualified
        // "here" reads as "the row I am on" and sends the user moving a
        // cursor that cannot affect the room.
        let hint = empty_room_hint(&fake_collaboration_participant("%1", "s1", None));
        assert!(hint.contains("main:agents"), "{hint}");
        assert!(hint.contains("not the selected row"), "{hint}");
    }

    #[test]
    fn the_empty_room_hint_falls_back_to_the_window_id() {
        let mut current = fake_collaboration_participant("%1", "s1", None);
        current.tmux_session_name = None;
        current.window_name = None;
        let hint = empty_room_hint(&current);
        assert!(hint.contains("@1"), "{hint}");
    }

    #[test]
    fn the_peer_hint_names_the_rows_that_can_receive() {
        // The table lists every agent on the host; without names the user
        // has no way to tell which handful of rows qualify.
        let hint = peer_choice_hint(&[
            "reviewer@%747 · callabo-set:0.1".to_string(),
            "codex@%751 · callabo-set:0.2".to_string(),
        ]);
        assert!(hint.contains("reviewer@%747 · callabo-set:0.1"), "{hint}");
        assert!(hint.contains("codex@%751 · callabo-set:0.2"), "{hint}");
    }

    #[test]
    fn the_peer_hint_stays_on_one_line_when_the_room_is_crowded() {
        let peers: Vec<String> = (1..=6).map(|n| format!("agent@%{n}")).collect();
        let hint = peer_choice_hint(&peers);
        assert!(hint.contains("+3 more"), "{hint}");
        assert!(
            !hint.contains("%6"),
            "trimmed peers must not be listed: {hint}"
        );
    }

    #[test]
    fn shift_tab_leaves_kind_cycling_to_tab_alone() {
        // Tab and Shift-Tab used to share an arm. Splitting them must not
        // cost Tab its job — the two are different axes, not one list.
        let mut app = collaboration_watch_app();
        open_watch_collaboration_composer(&mut app);

        assert!(matches!(
            handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app),
            Action::None
        ));
        assert!(matches!(
            app.collaboration_composer.as_ref().unwrap().target,
            CollaborationComposeTarget::Send {
                kind: RequestKind::Review,
                ..
            }
        ));
    }

    #[test]
    fn watch_collaboration_composer_badges_make_contract_changes_visible() {
        for (kind, icon, label, background) in [
            (RequestKind::Question, "?", "QUESTION", Color::Cyan),
            (RequestKind::Review, "◆", "REVIEW", Color::Magenta),
            (RequestKind::Task, "▶", "TASK", Color::Yellow),
            (RequestKind::Notice, "!", "NOTICE", Color::Blue),
        ] {
            let badge = request_kind_badge(kind);
            assert_eq!(
                (badge.icon, badge.label, badge.background),
                (icon, label, background)
            );
        }
        assert_eq!(work_mode_badge(WorkMode::ReadOnly).icon, "○");
        assert_eq!(work_mode_badge(WorkMode::ReadOnly).background, Color::Green);
        assert_eq!(work_mode_badge(WorkMode::Execute).icon, "●");
        assert_eq!(work_mode_badge(WorkMode::Execute).background, Color::Red);

        let mut app = collaboration_watch_app();
        open_watch_collaboration_composer(&mut app);
        let theme = watch_theme(WatchTheme::Classic);
        let composer = app.collaboration_composer.as_ref().unwrap();
        let (title, border) = collaboration_composer_title(composer, theme);
        let text = title
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("? QUESTION"));
        assert!(text.contains("○ READ-ONLY"));
        assert_eq!(border, Color::Cyan);

        handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app);
        handle_collaboration_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app);
        let composer = app.collaboration_composer.as_ref().unwrap();
        let (title, border) = collaboration_composer_title(composer, theme);
        let text = title
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("◆ REVIEW"));
        assert!(text.contains("● EXECUTE"));
        assert_eq!(border, Color::Red);
        assert_eq!(
            title
                .spans
                .iter()
                .find(|span| span.content.contains("REVIEW"))
                .and_then(|span| span.style.bg),
            Some(Color::Magenta)
        );
    }

    #[test]
    fn watch_mailbox_opens_reply_for_claimed_incoming_request() {
        let mut app = collaboration_watch_app();
        let room = app.collaboration.room.as_ref().unwrap();
        app.collaboration
            .incoming
            .push(fake_watch_collaboration_request(
                "req_watch_123456",
                room.peers[0].clone(),
                room.current.clone(),
                RequestStatus::Claimed,
            ));
        app.collaboration_mailbox.open = true;

        assert!(matches!(
            handle_collaboration_mailbox_event(KeyCode::Char('e'), &mut app),
            Action::None
        ));
        assert!(matches!(
            app.collaboration_composer
                .as_ref()
                .map(|composer| &composer.target),
            Some(CollaborationComposeTarget::Reply { request_id, .. })
                if request_id == "req_watch_123456"
        ));
    }

    #[test]
    fn watch_mailbox_render_contains_request_and_peer() {
        let mut app = collaboration_watch_app();
        let room = app.collaboration.room.as_ref().unwrap();
        let mut request = fake_watch_collaboration_request(
            "req_watch_render_123456",
            room.peers[0].clone(),
            room.current.clone(),
            RequestStatus::Claimed,
        );
        request.air_artifacts.push(AirArtifactReference {
            artifact_id: format!("urn:air:sha256:{}", "b".repeat(64)),
            profile: AirArtifactProfile::PlanNativeCli,
            label: Some("CAL-6924 execution plan".into()),
            locator: None,
        });
        app.collaboration.incoming.push(request);
        app.collaboration_mailbox.open = true;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(dump.contains("collaboration"));
        assert!(dump.contains("reviewer@%2"));
        assert!(dump.contains("AIR PLAN"));
        assert!(dump.contains("bbbbbbbbbbbb"));
        assert!(dump.contains("review the auth change"));
    }

    fn fake_session(id: &str, name: &str, attached_clients: u32) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            name: name.into(),
            attached_clients,
        }
    }

    fn fake_session_activity(
        id: &str,
        name: &str,
        total_attached_secs: u64,
        attached_since: Option<OffsetDateTime>,
    ) -> SessionActivity {
        SessionActivity {
            session_id: id.into(),
            name: name.into(),
            attached_clients: u32::from(attached_since.is_some()),
            total_attached_secs,
            attached_since,
            last_seen_at: OffsetDateTime::now_utc(),
        }
    }

    fn plain_text(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect()
    }

    fn display_col_of(haystack: &str, needle: &str) -> Option<usize> {
        haystack
            .find(needle)
            .map(|idx| unicode_width::UnicodeWidthStr::width(&haystack[..idx]))
    }

    #[test]
    fn render_does_not_panic_on_test_backend() {
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.set_data(
            vec![
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
            ],
            vec![
                fake_pane("%30", "work", 0, 0, "vim"),
                fake_pane("%31", "work", 1, 0, "cargo build"),
            ],
        );

        terminal.draw(|f| render(f, &mut app)).unwrap();
    }

    #[test]
    fn agents_group_by_session_then_window_pane_index() {
        // Agents from sessions "alpha" and "beta" interleaved by pane id,
        // plus one stale agent whose pane no longer exists. Expect:
        //   1. all alpha agents grouped, then all beta agents grouped
        //   2. within a session, ordered by window then pane index
        //   3. stale agent at the end
        //
        // Uses an explicit `[Session, Pane]` sort so the assertion stays
        // independent of `last_activity_at` jitter from `fake_agent`. The
        // default `[Session, Activity]` is exercised by separate tests.
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::Session, WatchSortKey::Pane],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mk = |session_id: &str, pane: &str| {
            fake_agent(
                session_id,
                Some(pane),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )
        };
        app.set_data(
            vec![
                mk("s-beta-1", "%50"),
                mk("s-alpha-2", "%20"),
                mk("s-stale", "%999"), // pane not in inventory
                mk("s-alpha-1", "%10"),
                mk("s-beta-2", "%40"),
            ],
            vec![
                fake_pane("%10", "alpha", 0, 0, "claude"),
                fake_pane("%20", "alpha", 1, 0, "claude"),
                fake_pane("%40", "beta", 0, 0, "claude"),
                fake_pane("%50", "beta", 0, 1, "claude"),
            ],
        );

        let agent_pane_ids: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();

        assert_eq!(
            agent_pane_ids,
            vec!["%10", "%20", "%40", "%50", "%999"],
            "expected grouping: alpha (w0p0, w1p0), beta (w0p0, w0p1), then stale"
        );
    }

    #[test]
    fn agent_window_pane_indices_sort_numerically_not_lex() {
        // "10" must sort AFTER "2" within a session — string comparison
        // would invert that. Regression guard for the parse::<u32>() path.
        // Uses explicit `Pane` sort so activity-jitter doesn't matter.
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::Session, WatchSortKey::Pane],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mk = |session_id: &str, pane: &str| {
            fake_agent(
                session_id,
                Some(pane),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )
        };
        app.set_data(
            vec![mk("a", "%2"), mk("a", "%10"), mk("a", "%1")],
            vec![
                fake_pane("%1", "main", 0, 0, "x"),
                fake_pane("%2", "main", 0, 1, "x"),
                fake_pane("%10", "main", 0, 10, "x"),
            ],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        assert_eq!(order, vec!["%1", "%2", "%10"]);
    }

    /// Build an agent with an explicit `last_activity_at` so sort tests
    /// don't depend on `fake_agent`'s wall-clock jitter.
    fn fake_agent_at(session_id: &str, pane: &str, last_activity_at: OffsetDateTime) -> Agent {
        let mut a = fake_agent(
            session_id,
            Some(pane),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        );
        a.last_activity_at = last_activity_at;
        a
    }

    #[test]
    fn summary_line_falls_through_recap_then_title_then_prompt() {
        let mut a = fake_agent_at("s", "%1", OffsetDateTime::now_utc());
        a.last_prompt = Some("do the thing".into());

        // Only a prompt: every mode shows it — the column never blanks.
        assert_eq!(summary_line(&a, WatchSummary::Recap), "do the thing");
        assert_eq!(summary_line(&a, WatchSummary::Title), "do the thing");
        assert_eq!(summary_line(&a, WatchSummary::Prompt), "do the thing");

        // A session title outranks the prompt, except in prompt-only mode.
        a.ai_title = Some("infra cleanup".into());
        assert_eq!(summary_line(&a, WatchSummary::Recap), "infra cleanup");
        assert_eq!(summary_line(&a, WatchSummary::Title), "infra cleanup");
        assert_eq!(summary_line(&a, WatchSummary::Prompt), "do the thing");

        // A recap outranks both, and collapses to its first line.
        a.recap = Some("Redis restored; MinIO left.\ndropped".into());
        assert_eq!(
            summary_line(&a, WatchSummary::Recap),
            "Redis restored; MinIO left."
        );
        assert_eq!(summary_line(&a, WatchSummary::Title), "infra cleanup");

        // Codex/Gemini shape: no recap or title source, and nothing typed
        // yet — renders the placeholder rather than an empty cell.
        a.recap = None;
        a.ai_title = None;
        a.last_prompt = None;
        assert_eq!(summary_line(&a, WatchSummary::Recap), "-");
    }

    #[test]
    fn default_sort_keeps_session_grouping_and_floats_latest_activity_in_each_group() {
        // Default config = [Session, Activity]. Two sessions with two
        // agents each at staggered timestamps. Expect:
        //   - alpha group first, then beta group (session asc)
        //   - newest agent at top within each group
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);
        let t3 = time::macros::datetime!(2026-04-28 12:00:00 UTC);

        let mut app = App::new();
        app.set_data(
            vec![
                fake_agent_at("a-old", "%10", t0),
                fake_agent_at("a-new", "%11", t2),
                fake_agent_at("b-old", "%20", t1),
                fake_agent_at("b-new", "%21", t3),
            ],
            vec![
                fake_pane("%10", "alpha", 0, 0, "claude"),
                fake_pane("%11", "alpha", 0, 1, "claude"),
                fake_pane("%20", "beta", 0, 0, "claude"),
                fake_pane("%21", "beta", 0, 1, "claude"),
            ],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        // alpha: %11 (newer t2) before %10 (older t0); then beta: %21
        // (newer t3) before %20 (older t1).
        assert_eq!(order, vec!["%11", "%10", "%21", "%20"]);
    }

    #[test]
    fn push_resorts_rows_so_a_state_change_moves_immediately() {
        // sort = [State, Activity]. A pushed `Error` transition must move the
        // row to its sorted position now, not on the next 5 s `Full` tick —
        // otherwise the badge updates but the row stays put ("한 박자 늦음").
        let t_old = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t_new = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::State, WatchSortKey::Activity],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent_at("a", "%1", t_old), // Idle, older
                fake_agent_at("b", "%2", t_new), // Idle, newer
            ],
            vec![
                fake_pane("%1", "main", 0, 0, "claude"),
                fake_pane("%2", "main", 0, 1, "claude"),
            ],
        );

        let order = |app: &App| -> Vec<String> {
            app.rows
                .iter()
                .filter_map(|r| match r {
                    WatchRow::Agent(a) => a.pane.clone(),
                    WatchRow::BarePane(_) | WatchRow::Session(_) => None,
                })
                .collect()
        };

        // Both Idle → newer activity floats first, so %2 leads, %1 trails.
        assert_eq!(order(&app), vec!["%2", "%1"]);

        // Push: agent "a" (pane %1) flips to Error (state rank 0 → top).
        let mut updated = fake_agent_at("a", "%1", t_old);
        updated.state = AgentState::Error;
        apply_outcome(&mut app, RefreshOutcome::SingleAgent(updated));

        // The re-sort ran as part of the push, so %1 is already on top.
        assert_eq!(order(&app), vec!["%1", "%2"]);
    }

    #[test]
    fn session_view_collapses_panes_to_latest_active_agent() {
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![
                fake_agent_at("old", "%1", t0),
                fake_agent_at("new", "%2", t1),
            ],
            vec![
                fake_pane("%1", "main", 0, 0, "claude"),
                fake_pane("%2", "main", 0, 1, "codex"),
                fake_pane("%3", "side", 0, 0, "vim"),
            ],
            vec![fake_session("$1", "main", 1), fake_session("$2", "side", 0)],
            vec![],
        );

        assert_eq!(app.rows.len(), 2);
        let WatchRow::Session(main) = &app.rows[0] else {
            panic!("expected session row");
        };
        assert_eq!(main.session, "main");
        assert_eq!(main.representative_pane.as_deref(), Some("%2"));
        assert_eq!(
            main.latest_agent.as_ref().map(|a| a.session_id.as_str()),
            Some("new")
        );
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));
    }

    /// Regression: a full refresh that reorders the list must not drag the
    /// highlight onto a neighbouring session.
    ///
    /// Reported as "the list looks shifted by one — I hit Enter twice on
    /// `muxa` and the `amux` session opened". `set_data_with_sessions`
    /// rebuilds and re-sorts on every refresh (~2 Hz), and the default sort
    /// leads with `state`; when `amux` goes Error it jumps above `muxa`, so
    /// holding the raw table index left the cursor on `amux`.
    #[test]
    fn full_refresh_reorder_keeps_the_cursor_on_the_same_session() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![
                WatchSortKey::State,
                WatchSortKey::Session,
                WatchSortKey::Activity,
            ],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);

        let panes = || {
            vec![
                fake_pane("%1", "amux", 0, 0, "claude"),
                fake_pane("%2", "muxa", 0, 0, "claude"),
            ]
        };
        let sessions = || vec![fake_session("$1", "amux", 0), fake_session("$2", "muxa", 0)];
        let session_names = |app: &App| -> Vec<String> {
            app.rows
                .iter()
                .filter_map(|r| match r {
                    WatchRow::Session(s) => Some(s.session.clone()),
                    WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
                })
                .collect()
        };

        // `muxa` is blocked on an error, so it leads; `amux` is idle below it.
        let mut muxa_agent = fake_agent_at("m", "%2", now);
        muxa_agent.state = AgentState::Error;
        app.set_data_with_sessions(
            vec![fake_agent_at("a", "%1", now), muxa_agent],
            panes(),
            sessions(),
            vec![],
        );
        assert_eq!(session_names(&app), vec!["muxa", "amux"]);

        // The user puts the cursor on `muxa` (row 0 here).
        app.table_state.select(Some(0));
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));

        // Next refresh: `muxa` recovers to Idle and `amux` errors, so the two
        // rows swap. The cursor must travel with `muxa`, not stay on row 0.
        let mut amux_agent = fake_agent_at("a", "%1", now);
        amux_agent.state = AgentState::Error;
        app.set_data_with_sessions(
            vec![amux_agent, fake_agent_at("m", "%2", now)],
            panes(),
            sessions(),
            vec![],
        );
        assert_eq!(session_names(&app), vec!["amux", "muxa"]);
        assert_eq!(app.table_state.selected(), Some(1));
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));
        // …and Enter therefore attaches to the pane the user is looking at.
        assert!(
            matches!(quick_prompt_action(&app), Action::AttachPane(p) if p == "%2"),
            "Enter must target the highlighted session's pane"
        );
    }

    /// A session row's `representative_pane` is whichever of its agents was
    /// most recently active, so it changes on its own. Pinning the cursor by
    /// pane id alone would lose the row; identity is pinned by session name.
    #[test]
    fn refresh_keeps_the_cursor_when_the_representative_pane_changes() {
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Activity],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let panes = vec![
            fake_pane("%1", "side", 0, 0, "claude"),
            fake_pane("%2", "muxa", 0, 0, "claude"),
            fake_pane("%3", "muxa", 0, 1, "codex"),
        ];
        let sessions = vec![fake_session("$1", "side", 0), fake_session("$2", "muxa", 0)];

        app.set_data_with_sessions(
            vec![
                fake_agent_at("s", "%1", t0),
                fake_agent_at("m1", "%2", t1),
                fake_agent_at("m2", "%3", t0),
            ],
            panes.clone(),
            sessions.clone(),
            vec![],
        );
        assert_eq!(app.table_state.selected(), Some(0));
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));

        // `side` becomes the most recently active session so the rows swap,
        // and inside `muxa` the codex pane %3 overtakes %2 as representative.
        // Both moved — the cursor still belongs on `muxa`.
        app.set_data_with_sessions(
            vec![
                fake_agent_at("s", "%1", t2),
                fake_agent_at("m1", "%2", t0),
                fake_agent_at("m2", "%3", t1),
            ],
            panes,
            sessions,
            vec![],
        );
        assert_eq!(app.table_state.selected(), Some(1));
        let WatchRow::Session(selected) = app.selected_row().expect("selection survives") else {
            panic!("expected session row");
        };
        assert_eq!(selected.session, "muxa");
        assert_eq!(selected.representative_pane.as_deref(), Some("%3"));
    }

    #[test]
    fn session_view_label_summarizes_agent_states() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mut working = fake_agent_at("working", "%1", now);
        working.state = AgentState::Working;
        let mut working_2 = fake_agent_at("working-2", "%5", now);
        working_2.state = AgentState::Working;
        let mut waiting = fake_agent_at("waiting", "%2", now);
        waiting.state = AgentState::WaitingInput;
        let mut idle = fake_agent_at("idle", "%3", now);
        idle.state = AgentState::Idle;
        let mut error = fake_agent_at("error", "%4", now);
        error.state = AgentState::Error;

        app.set_data_with_sessions(
            vec![working, working_2, waiting, idle, error],
            vec![
                fake_pane("%1", "main", 0, 0, "claude"),
                fake_pane("%2", "main", 0, 1, "codex"),
                fake_pane("%3", "main", 0, 2, "gemini"),
                fake_pane("%4", "main", 0, 3, "claude"),
                fake_pane("%5", "main", 0, 4, "codex"),
            ],
            vec![fake_session("$1", "main", 1)],
            vec![],
        );

        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        let text = WatchColumn::Pane.session_text(
            row,
            now,
            &app.panes,
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            app.watch_cfg.summary,
        );
        assert_eq!(plain_text(&text), "■ +4  main");
    }

    #[test]
    fn session_view_state_summary_survives_long_session_name() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            // Assert the static gutter layout, not an animation frame.
            spinner: false,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let session = "a-very-long-session-name-that-would-otherwise-eat-state-markers";
        let mut working = fake_agent_at("working", "%1", now);
        working.state = AgentState::Working;
        let mut waiting = fake_agent_at("waiting", "%2", now);
        waiting.state = AgentState::WaitingInput;
        let mut idle = fake_agent_at("idle", "%3", now);
        idle.state = AgentState::Idle;

        app.set_data_with_sessions(
            vec![working, waiting, idle],
            vec![
                fake_pane("%1", session, 0, 0, "claude"),
                fake_pane("%2", session, 0, 1, "codex"),
                fake_pane("%3", session, 0, 2, "gemini"),
            ],
            vec![fake_session("$1", session, 1)],
            vec![],
        );

        let backend = TestBackend::new(64, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let screen = (0..terminal.backend().buffer().area().height)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("▶ ● ○"),
            "state summary should remain visible before a clipped long session name:\n{screen}"
        );
    }

    #[test]
    fn swarm_view_renders_cluster_spinner_and_subagent_tree() {
        let cfg = WatchConfig {
            view: WatchView::Swarm,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mut agent = fake_agent(
            "worker",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("refactor the reconciler"),
            Some("Opus"),
            Some(17.0),
            Some(1.0),
        );
        agent.subagents = vec![muxa::state::Subagent {
            kind: "Explore".into(),
            description: Some("map the codebase".into()),
            started_at: OffsetDateTime::now_utc(),
        }];
        app.set_data_with_sessions(
            vec![agent],
            vec![fake_pane("%1", "worker", 0, 0, "claude")],
            vec![fake_session("$1", "worker", 1)],
            vec![],
        );

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let screen = (0..terminal.backend().buffer().area().height)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("Swarm"), "swarm title:\n{screen}");
        assert!(
            screen.contains("worker"),
            "session cluster header:\n{screen}"
        );
        assert!(screen.contains("Explore"), "subagent tree row:\n{screen}");
        assert!(screen.contains("◇1"), "subagent load badge:\n{screen}");
    }

    #[test]
    fn session_view_shows_single_agent_state_summary() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mut agent = fake_agent_at("agent", "%1", now);
        agent.state = AgentState::Idle;
        app.set_data_with_sessions(
            vec![agent.clone()],
            vec![fake_pane("%1", "main", 0, 0, "claude")],
            vec![fake_session("$1", "main", 1)],
            vec![],
        );

        agent.state = AgentState::WaitingInput;
        agent.last_activity_at = now + time::Duration::seconds(1);
        apply_single_agent(&mut app, agent);

        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        let text = WatchColumn::Pane.session_text(
            row,
            now + time::Duration::seconds(1),
            &app.panes,
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            app.watch_cfg.summary,
        );
        assert_eq!(plain_text(&text), "▶     main");
    }

    #[test]
    fn session_view_state_gutter_keeps_names_aligned() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mut multi_working = fake_agent_at("multi-working", "%1", now);
        multi_working.state = AgentState::Working;
        let mut multi_waiting = fake_agent_at("multi-waiting", "%2", now);
        multi_waiting.state = AgentState::WaitingInput;
        let mut single_idle = fake_agent_at("single-idle", "%3", now);
        single_idle.state = AgentState::Idle;

        app.set_data_with_sessions(
            vec![multi_working, multi_waiting, single_idle],
            vec![
                fake_pane("%1", "multi", 0, 0, "claude"),
                fake_pane("%2", "multi", 0, 1, "codex"),
                fake_pane("%3", "single", 0, 0, "claude"),
            ],
            vec![
                fake_session("$1", "multi", 1),
                fake_session("$2", "single", 1),
            ],
            vec![],
        );

        let labels = app
            .rows
            .iter()
            .filter_map(|row| match row {
                WatchRow::Session(row) => {
                    let text = WatchColumn::Pane.session_text(
                        row,
                        now,
                        &app.panes,
                        watch_theme(WatchTheme::Classic),
                        Spinner::OFF,
                        app.watch_cfg.summary,
                    );
                    Some((row.session.as_str(), plain_text(&text)))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();

        assert_eq!(
            display_col_of(&labels["multi"], "multi"),
            Some(SESSION_STATE_GUTTER_WIDTH)
        );
        assert_eq!(
            display_col_of(&labels["single"], "single"),
            Some(SESSION_STATE_GUTTER_WIDTH)
        );
        assert_eq!(labels["multi"], "▶ ●   multi");
        assert_eq!(labels["single"], "○     single");
    }

    #[test]
    fn session_view_state_gutter_compresses_overflow_before_name() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let states = [
            AgentState::Error,
            AgentState::WaitingInput,
            AgentState::WaitingChoice,
            AgentState::Working,
            AgentState::Starting,
            AgentState::Idle,
            AgentState::Stopped,
        ];
        let agents = states
            .into_iter()
            .enumerate()
            .map(|(i, state)| {
                let mut agent = fake_agent_at(&format!("agent-{i}"), &format!("%{i}"), now);
                agent.state = state;
                agent
            })
            .collect::<Vec<_>>();
        let panes = (0..states.len())
            .map(|i| {
                fake_pane(
                    &format!("%{i}"),
                    "crowded",
                    0,
                    u32::try_from(i).expect("fixture index fits u32"),
                    "claude",
                )
            })
            .collect::<Vec<_>>();

        app.set_data_with_sessions(
            agents,
            panes,
            vec![fake_session("$1", "crowded", 1)],
            vec![],
        );

        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        let text = WatchColumn::Pane.session_text(
            row,
            now,
            &app.panes,
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            app.watch_cfg.summary,
        );
        let label = plain_text(&text);
        assert_eq!(
            display_col_of(&label, "crowded"),
            Some(SESSION_STATE_GUTTER_WIDTH)
        );
        assert_eq!(label, "■ +6  crowded");
    }

    #[test]
    fn session_view_adds_attached_time_column_and_renders_total() {
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![],
            vec![fake_pane("%1", "main", 0, 0, "zsh")],
            vec![fake_session("$1", "main", 1)],
            vec![fake_session_activity(
                "$1",
                "main",
                3_600,
                Some(now - time::Duration::minutes(5)),
            )],
        );

        assert!(app.columns.contains(&WatchColumn::SessionTime));
        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        let text = WatchColumn::SessionTime.session_text(
            row,
            now,
            &app.panes,
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            app.watch_cfg.summary,
        );
        let cell = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert_eq!(cell, "1h05m");
    }

    #[test]
    fn herdr_session_row_shows_label_and_resolves_duration_by_workspace_id() {
        // herdr shape: the pane's session and the ledger key are both the
        // stable `workspace_id` (`w1`); the SessionInfo carries the human
        // label as its name (session_id == the workspace id). The row must
        // display the label, and the DUR column must resolve via the
        // session-id fallback even though no session *name* matches `w1`.
        let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![],
            vec![fake_pane("herdr:p1", "w1", 0, 0, "zsh")],
            vec![fake_session("w1", "muxa", 0)],
            vec![fake_session_activity(
                "w1",
                "muxa",
                3_600,
                Some(now - time::Duration::minutes(5)),
            )],
        );

        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        assert_eq!(row.session, "w1", "group key stays the workspace id");
        assert_eq!(row.display_name, "muxa", "display name is the label");

        let label = plain_text(&session_label(
            row,
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
        ));
        assert!(
            label.contains("muxa"),
            "label renders the workspace label: {label}"
        );

        let text = WatchColumn::SessionTime.session_text(
            row,
            now,
            &app.panes,
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            app.watch_cfg.summary,
        );
        let cell = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert_eq!(cell, "1h05m", "DUR resolves by workspace id");
    }

    #[test]
    fn herdr_session_row_falls_back_to_workspace_id_without_label() {
        // No SessionInfo for the pane's workspace (e.g. the workspace list
        // was unreachable this refresh): the display name degrades to the id.
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![],
            vec![fake_pane("herdr:p1", "w1", 0, 0, "zsh")],
            vec![],
            vec![],
        );

        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        assert_eq!(row.display_name, "w1", "display name falls back to the id");
    }

    #[test]
    fn same_named_tmux_session_and_herdr_workspace_stay_distinct_rows() {
        // A tmux session named "w1" and a herdr workspace whose id is "w1"
        // share a raw session string. Grouped by the raw id alone they'd merge
        // into one corrupted row (panes from both hosts, wrong count); the
        // host-namespaced group key keeps them apart.
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![],
            vec![
                // tmux "w1": two panes.
                fake_pane("%1", "w1", 0, 0, "zsh"),
                fake_pane("%2", "w1", 0, 1, "nvim"),
                // herdr workspace "w1": one pane.
                fake_pane("herdr:p1", "w1", 0, 0, "zsh"),
            ],
            vec![fake_session("$1", "w1", 1)],
            vec![],
        );

        let session_rows: Vec<&SessionRow> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Session(s) => Some(s.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            session_rows.len(),
            2,
            "tmux w1 and herdr w1 must be two distinct rows"
        );

        let tmux_row = session_rows
            .iter()
            .find(|s| s.group_key == "tmux:w1")
            .expect("tmux:w1 row present");
        let herdr_row = session_rows
            .iter()
            .find(|s| s.group_key == "herdr:w1")
            .expect("herdr:w1 row present");

        // Raw session id stays "w1" on both (display/ledger key), only the
        // group key is namespaced.
        assert_eq!(tmux_row.session, "w1");
        assert_eq!(herdr_row.session, "w1");
        assert_eq!(tmux_row.pane_count, 2, "tmux row keeps its two panes");
        assert_eq!(herdr_row.pane_count, 1, "herdr row keeps its one pane");
    }

    // ---- multi-host aggregation + badges ---------------------------------

    #[test]
    fn row_host_classifies_by_pane_namespace() {
        let tmux = WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh")));
        let herdr = WatchRow::BarePane(Box::new(fake_pane("herdr:p1", "w1", 0, 0, "zsh")));
        let zellij = WatchRow::BarePane(Box::new(fake_pane("zellij:7", "z", 0, 0, "zsh")));
        let legacy = WatchRow::BarePane(Box::new(fake_pane("weird-id", "x", 0, 0, "zsh")));
        assert_eq!(row_host(&tmux), Some(muxa::HostKind::Tmux));
        assert_eq!(row_host(&herdr), Some(muxa::HostKind::Herdr));
        assert_eq!(row_host(&zellij), Some(muxa::HostKind::Zellij));
        assert_eq!(row_host(&legacy), None, "unrecognized ids get no badge");
    }

    #[test]
    fn rows_multi_host_only_when_hosts_differ() {
        let tmux_a = WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh")));
        let tmux_b = WatchRow::BarePane(Box::new(fake_pane("%2", "main", 0, 1, "zsh")));
        let herdr = WatchRow::BarePane(Box::new(fake_pane("herdr:p1", "w1", 0, 0, "zsh")));
        let legacy = WatchRow::BarePane(Box::new(fake_pane("weird", "x", 0, 0, "zsh")));

        assert!(
            !rows_multi_host(std::slice::from_ref(&tmux_a)),
            "single tmux host → no badges"
        );
        assert!(
            !rows_multi_host(&[
                WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh"))),
                WatchRow::BarePane(Box::new(fake_pane("%2", "main", 0, 1, "zsh"))),
            ]),
            "two tmux rows are still one host"
        );
        assert!(
            rows_multi_host(&[
                WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh"))),
                WatchRow::BarePane(Box::new(fake_pane("herdr:p1", "w1", 0, 0, "zsh"))),
            ]),
            "tmux + herdr → badges on"
        );
        // Unclassifiable rows never flip the decision on their own.
        assert!(!rows_multi_host(&[legacy]));
        drop((tmux_a, tmux_b, herdr));
    }

    #[test]
    fn mixed_host_panes_build_distinct_session_rows() {
        // A tmux session and a herdr workspace with colliding-looking keys
        // ("main" vs "w1") stay separate rows, each classified to its host.
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![],
            vec![
                fake_pane("%1", "main", 0, 0, "vim"),
                fake_pane("herdr:p1", "w1", 0, 0, "zsh"),
            ],
            vec![fake_session("w1", "muxa", 0)],
            vec![],
        );
        assert_eq!(app.rows.len(), 2, "one row per host session");
        let hosts: std::collections::HashSet<_> = app.rows.iter().filter_map(row_host).collect();
        assert!(hosts.contains(&muxa::HostKind::Tmux));
        assert!(hosts.contains(&muxa::HostKind::Herdr));
        assert!(rows_multi_host(&app.rows));
    }

    #[test]
    fn host_badges_render_only_in_multi_host() {
        fn render_to_string(app: &mut App) -> String {
            let backend = TestBackend::new(120, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| render(f, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect()
        }

        // Multi-host: the SESSION cells carry dim "tmux"/"herdr" tags. The
        // session names ("main"/"w1") don't contain those words, so a match
        // can only come from the badge.
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut multi = App::with_config(cfg.clone());
        multi.set_data_with_sessions(
            vec![],
            vec![
                fake_pane("%1", "main", 0, 0, "vim"),
                fake_pane("herdr:p1", "w1", 0, 0, "zsh"),
            ],
            vec![],
            vec![],
        );
        let text = render_to_string(&mut multi);
        assert!(
            text.contains("tmux"),
            "multi-host shows tmux badge: {text:?}"
        );
        assert!(
            text.contains("herdr"),
            "multi-host shows herdr badge: {text:?}"
        );

        // Single-host: no badge — a lone tmux session must not gain a
        // "herdr" (or any) host tag.
        let mut single = App::with_config(cfg);
        single.set_data_with_sessions(
            vec![],
            vec![fake_pane("%1", "main", 0, 0, "vim")],
            vec![],
            vec![],
        );
        let text = render_to_string(&mut single);
        assert!(
            !text.contains("herdr"),
            "single-host adds no host badge: {text:?}"
        );
    }

    #[test]
    fn session_view_keeps_duration_column_visible_at_80_cols() {
        let cfg = WatchConfig {
            view: WatchView::Session,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        assert_eq!(
            app.columns,
            vec![
                WatchColumn::Pane,
                WatchColumn::StateAge,
                WatchColumn::SessionTime,
                WatchColumn::Activity,
                WatchColumn::Prompt,
            ]
        );
        app.set_data_with_sessions(
            vec![],
            vec![fake_pane("%1", "main", 0, 0, "zsh")],
            vec![fake_session("$1", "main", 0)],
            vec![fake_session_activity("$1", "main", 3_900, None)],
        );

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let screen = (0..buf.area().height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("DUR"), "{screen}");
        assert!(screen.contains("1h05m"), "{screen}");
    }

    #[test]
    fn session_view_explicit_state_column_is_preserved() {
        let cfg = WatchConfig {
            view: WatchView::Session,
            columns: vec!["pane".into(), "state".into(), "prompt".into()],
            ..WatchConfig::default()
        };
        let app = App::with_config(cfg);
        assert_eq!(
            app.columns,
            vec![
                WatchColumn::Pane,
                WatchColumn::State,
                WatchColumn::SessionTime,
                WatchColumn::Prompt,
            ]
        );
    }

    #[test]
    fn activity_only_sort_floats_globally_newest_agent_to_the_top() {
        // sort = [Activity] — drops session grouping entirely. Expected
        // order is strict newest-first across all sessions.
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);

        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::Activity],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent_at("alpha", "%10", t0),
                fake_agent_at("beta", "%20", t2),
                fake_agent_at("gamma", "%30", t1),
            ],
            vec![
                fake_pane("%10", "alpha", 0, 0, "x"),
                fake_pane("%20", "beta", 0, 0, "x"),
                fake_pane("%30", "gamma", 0, 0, "x"),
            ],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        assert_eq!(order, vec!["%20", "%30", "%10"]);
    }

    #[test]
    fn state_sort_prioritizes_rows_that_need_attention() {
        let now = OffsetDateTime::now_utc();
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::State],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let mut idle = fake_agent_at("idle", "%10", now);
        idle.state = AgentState::Idle;
        let mut working = fake_agent_at("working", "%20", now);
        working.state = AgentState::Working;
        let mut choice = fake_agent_at("choice", "%30", now);
        choice.state = AgentState::WaitingChoice;
        let mut error = fake_agent_at("error", "%40", now);
        error.state = AgentState::Error;

        app.set_data(
            vec![idle, working, choice, error],
            vec![
                fake_pane("%10", "a", 0, 0, "x"),
                fake_pane("%20", "a", 0, 1, "x"),
                fake_pane("%30", "a", 0, 2, "x"),
                fake_pane("%40", "a", 0, 3, "x"),
            ],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        assert_eq!(order, vec!["%40", "%30", "%20", "%10"]);
    }

    #[test]
    fn session_view_duration_sort_orders_longest_session_first() {
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::SessionTime],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data_with_sessions(
            vec![],
            vec![
                fake_pane("%1", "short", 0, 0, "zsh"),
                fake_pane("%2", "long", 0, 0, "zsh"),
            ],
            vec![
                fake_session("$1", "short", 0),
                fake_session("$2", "long", 0),
            ],
            vec![
                fake_session_activity("$1", "short", 60, None),
                fake_session_activity("$2", "long", 3_600, None),
            ],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Session(s) => Some(s.session.as_str()),
                WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
            })
            .collect();
        assert_eq!(order, vec!["long", "short"]);
    }

    #[test]
    fn runtime_sort_preset_reorders_rows_and_preserves_selected_pane() {
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent_at("alpha", "%10", t0),
                fake_agent_at("beta", "%20", t1),
            ],
            vec![
                fake_pane("%10", "alpha", 0, 0, "x"),
                fake_pane("%20", "beta", 0, 0, "x"),
            ],
        );
        app.table_state.select(Some(0));

        app.apply_sort_preset(WatchSortPreset::Latest);

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        assert_eq!(order, vec!["%20", "%10"]);
        assert_eq!(app.selected_pane().as_deref(), Some("%10"));
    }

    #[test]
    fn sort_keybindings_switch_sort_presets() {
        let mut app = App::new();
        for (key, preset) in [
            ('s', WatchSortPreset::Session),
            ('l', WatchSortPreset::Latest),
            ('d', WatchSortPreset::Duration),
            ('t', WatchSortPreset::State),
        ] {
            let action = handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT)),
                &mut app,
            );
            assert!(matches!(action, Action::SetSort(p) if p == preset));
        }
    }

    #[test]
    fn persist_watch_sort_creates_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");

        persist_watch_sort(&path, &[WatchSortKey::Activity]).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("[watch]"), "{saved}");
        assert!(saved.contains("sort = [\"latest\"]"), "{saved}");
        let cfg = muxa::config::Config::load_or_default(Some(&path)).unwrap();
        assert_eq!(cfg.watch.sort, vec![WatchSortKey::Activity]);
    }

    #[test]
    fn persist_watch_sort_updates_sort_without_rewriting_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"# user config
[ui]
theme = "focus"

[watch]
columns = ["pane", "state"]
sort = ["state"]
"#,
        )
        .unwrap();

        persist_watch_sort(&path, &[WatchSortKey::State, WatchSortKey::Activity]).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# user config"), "{saved}");
        assert!(saved.contains("theme = \"focus\""), "{saved}");
        assert!(saved.contains("columns = [\"pane\", \"state\"]"), "{saved}");
        assert!(saved.contains("sort = [\"state\", \"latest\"]"), "{saved}");
        let cfg = muxa::config::Config::load_or_default(Some(&path)).unwrap();
        assert_eq!(
            cfg.watch.sort,
            vec![WatchSortKey::State, WatchSortKey::Activity]
        );
    }

    #[test]
    fn pane_id_sort_produces_lexicographic_order() {
        // sort = [PaneId] — useful for screenshots / docs where stable
        // alphabetic order is preferred over recency.
        let now = OffsetDateTime::now_utc();
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent_at("a", "%30", now),
                fake_agent_at("a", "%1", now),
                fake_agent_at("a", "%200", now),
            ],
            vec![
                fake_pane("%1", "a", 0, 0, "x"),
                fake_pane("%30", "a", 0, 1, "x"),
                fake_pane("%200", "a", 0, 2, "x"),
            ],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        // Lexicographic — "%1" < "%200" < "%30" because '2' < '3'.
        assert_eq!(order, vec!["%1", "%200", "%30"]);
    }

    #[test]
    fn stale_agents_always_sink_to_bottom_regardless_of_sort_keys() {
        // Stale = pane no longer in the inventory. Even when the sort
        // key would otherwise float them up (e.g. very recent activity),
        // the live/stale split takes precedence.
        let now = OffsetDateTime::now_utc();
        let very_recent = now;
        let older_at = now - time::Duration::hours(1);

        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::Activity],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent_at("stale-but-recent", "%999", very_recent),
                fake_agent_at("live-but-older", "%10", older_at),
            ],
            vec![fake_pane("%10", "main", 0, 0, "claude")],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                WatchRow::BarePane(_) | WatchRow::Session(_) => None,
            })
            .collect();
        assert_eq!(order, vec!["%10", "%999"]);
    }

    #[test]
    fn bare_panes_appear_after_agents_and_dedupe_by_pane_id() {
        let mut app = App::new();
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%10"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            vec![
                fake_pane("%10", "main", 0, 0, "claude"), // dedupes (matches agent)
                fake_pane("%99", "side", 2, 1, "vim"),
            ],
        );
        assert_eq!(app.rows.len(), 2);
        assert!(matches!(app.rows[0], WatchRow::Agent(_)));
        assert!(matches!(app.rows[1], WatchRow::BarePane(_)));
        // selection works across both kinds
        assert_eq!(app.selected_pane().as_deref(), Some("%10"));
        app.move_down();
        assert_eq!(app.selected_pane().as_deref(), Some("%99"));
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
        // Lock to PaneId sort so this test stays focused on the
        // selected_pane() contract — not on the default sort behaviour
        // covered by other tests.
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
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
            ],
            vec![],
        );
        assert_eq!(app.selected_pane().as_deref(), Some("%1"));
        app.move_down();
        assert_eq!(app.selected_pane().as_deref(), Some("%22"));
    }

    #[test]
    fn pane_display_falls_back_to_raw_id_when_not_in_cache() {
        // Empty pane cache simulates "no tmux running" or a stale agent
        // pane id; we expect the raw id back.
        let panes: Vec<PaneInfo> = Vec::new();
        assert_eq!(pane_display(Some("%9999"), &panes), "%9999");
        // None now renders explicitly as "(no pane)" to surface the
        // attach-unavailable case visually rather than mimicking a
        // generic "no data" dash.
        assert_eq!(pane_display(None, &panes), "(no pane)");
    }

    #[test]
    fn pane_display_resolves_against_cached_panes() {
        let panes = vec![PaneInfo {
            socket: None,
            pane_id: "%42".into(),
            session_id: String::new(),
            session: "main".into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "1".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: String::new(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }];
        assert_eq!(pane_display(Some("%42"), &panes), "main:1.0");
        // Misses fall through to the raw id without panicking.
        assert_eq!(pane_display(Some("%missing"), &panes), "%missing");
    }

    #[test]
    fn selection_movement_wraps_at_boundaries() {
        let mut app = App::new();
        app.set_data(
            vec![
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
            ],
            vec![],
        );
        assert_eq!(app.table_state.selected(), Some(0));
        app.move_down();
        assert_eq!(app.table_state.selected(), Some(1));
        // bottom → top
        app.move_down();
        assert_eq!(app.table_state.selected(), Some(0));
        // top → bottom
        app.move_up();
        assert_eq!(app.table_state.selected(), Some(1));
        app.move_up();
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn initial_pane_preselects_matching_row_on_first_load() {
        // Lock to PaneId sort: this test cares about which row matches
        // `set_initial_pane`, not the default sort interleaving.
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_initial_pane(Some("%2".into()));
        app.set_data(
            vec![
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
                fake_agent(
                    "s3",
                    Some("%3"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![],
        );
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));

        // Hint is one-shot: a refresh that brings new rows must not re-snap
        // the cursor away from where the user moved it.
        app.move_down();
        assert_eq!(app.selected_pane().as_deref(), Some("%3"));
        app.set_data(
            vec![
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
                fake_agent(
                    "s3",
                    Some("%3"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![],
        );
        assert_eq!(app.selected_pane().as_deref(), Some("%3"));
    }

    #[test]
    fn render_scrolls_viewport_to_initial_pane_when_far_down_the_list() {
        // Reproduces the user-visible scenario: 30 agents listed, the
        // active pane is way below the viewport. We need the table to
        // auto-scroll so the highlighted row is actually on screen.
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();

        let agents: Vec<Agent> = (0..30)
            .map(|i| {
                let pane = format!("%{i}");
                let session = format!("s{i:02}");
                fake_agent(
                    &session,
                    Some(&pane),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                )
            })
            .collect();

        app.set_initial_pane(Some("%25".into()));
        app.set_data(agents, vec![]);
        assert_eq!(app.selected_pane().as_deref(), Some("%25"));

        terminal.draw(|f| render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let mut dump = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                dump.push_str(buf[(x, y)].symbol());
            }
            dump.push('\n');
        }

        assert!(
            dump.contains("%25"),
            "expected the selected '%25' row to be scrolled into view, got:\n{dump}",
        );
    }

    #[test]
    fn initial_pane_falls_back_to_row_zero_when_unknown() {
        let mut app = App::new();
        app.set_initial_pane(Some("%does-not-exist".into()));
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            vec![],
        );
        assert_eq!(app.selected_pane().as_deref(), Some("%1"));
    }

    #[test]
    fn relative_time_buckets() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(relative_time(now, now), "0s");
        assert_eq!(relative_time(now - time::Duration::minutes(5), now), "5m");
        assert_eq!(relative_time(now - time::Duration::hours(3), now), "3h");
        assert_eq!(relative_time(now - time::Duration::days(3), now), "3d");
    }

    #[test]
    fn default_columns_are_prompt_forward() {
        let app = App::new();
        assert_eq!(
            app.columns,
            vec![
                WatchColumn::Pane,
                WatchColumn::StateAge,
                WatchColumn::Activity,
                WatchColumn::Prompt,
            ]
        );
    }

    #[test]
    fn custom_columns_resolve_in_config_order() {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            columns: vec!["prompt".into(), "pane".into(), "kind".into()],
            widths: HashMap::new(),
            ..Default::default()
        };
        let app = App::with_config(cfg);
        assert_eq!(
            app.columns,
            vec![WatchColumn::Prompt, WatchColumn::Pane, WatchColumn::Kind]
        );
    }

    #[test]
    fn unknown_column_keys_are_skipped() {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            columns: vec!["pane".into(), "bogus".into(), "prompt".into()],
            widths: HashMap::new(),
            ..Default::default()
        };
        let app = App::with_config(cfg);
        // "bogus" was warned-and-skipped; the rest survive in order.
        assert_eq!(app.columns, vec![WatchColumn::Pane, WatchColumn::Prompt]);
    }

    #[test]
    fn width_spec_kinds_translate_to_constraints() {
        let mut widths = HashMap::new();
        widths.insert("pane".into(), WidthSpec::Length(40));
        widths.insert("prompt".into(), WidthSpec::Min(50));
        widths.insert("kind".into(), WidthSpec::Percentage(20));
        widths.insert("state".into(), WidthSpec::Invalid("nope".into()));
        let cfg = WatchConfig {
            view: WatchView::Pane,
            columns: vec![
                "pane".into(),
                "prompt".into(),
                "kind".into(),
                "state".into(),
            ],
            widths,
            ..Default::default()
        };
        assert_eq!(
            resolve_width(WatchColumn::Pane, &cfg),
            Constraint::Length(40)
        );
        assert_eq!(
            resolve_width(WatchColumn::Prompt, &cfg),
            Constraint::Min(50)
        );
        assert_eq!(
            resolve_width(WatchColumn::Kind, &cfg),
            Constraint::Percentage(20)
        );
        // Invalid -> column default.
        assert_eq!(
            resolve_width(WatchColumn::State, &cfg),
            WatchColumn::State.default_width()
        );
        // Missing -> column default.
        assert_eq!(
            resolve_width(WatchColumn::Activity, &cfg),
            WatchColumn::Activity.default_width()
        );
    }

    #[test]
    fn agent_text_renders_for_each_column_kind() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hello world"),
            Some("Opus"),
            Some(42.0),
            Some(0.34),
        );
        // Smoke-test that every column variant produces a Text without panic.
        for col in [
            WatchColumn::Pane,
            WatchColumn::Kind,
            WatchColumn::State,
            WatchColumn::Model,
            WatchColumn::Ctx,
            WatchColumn::Cost,
            WatchColumn::Limits,
            WatchColumn::Prompt,
            WatchColumn::Activity,
            WatchColumn::SessionTime,
        ] {
            let _ = col.agent_text(
                &a,
                now,
                &[],
                watch_theme(WatchTheme::Classic),
                Spinner::OFF,
                WatchSummary::default(),
            );
        }
    }

    /// Helper: pull the rendered LIMITS cell down to a single concatenated
    /// string so tests can assert on substrings without juggling spans.
    fn limits_cell_string(a: &Agent, now: OffsetDateTime) -> String {
        let text = WatchColumn::Limits.agent_text(
            a,
            now,
            &[],
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            WatchSummary::default(),
        );
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>()
    }

    /// Helper: pull the foreground colour of the first span — sufficient
    /// for the cell tests since LIMITS only ever emits a single span.
    fn limits_cell_fg(a: &Agent, now: OffsetDateTime) -> Option<Color> {
        WatchColumn::Limits
            .agent_text(
                a,
                now,
                &[],
                watch_theme(WatchTheme::Classic),
                Spinner::OFF,
                WatchSummary::default(),
            )
            .lines
            .first()
            .and_then(|l| l.spans.first())
            .and_then(|s| s.style.fg)
    }

    #[test]
    fn limits_renders_red_cap_badge_when_rate_limit_active_with_reset_time() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        );
        a.rate_limited_until = Some(now + time::Duration::hours(2) + time::Duration::minutes(14));
        a.rate_limit_scope = Some(RateLimitScope::FiveHour);
        let s = limits_cell_string(&a, now);
        assert_eq!(s, "⛔ 5h in 2h 14m", "got {s:?}");
        assert_eq!(limits_cell_fg(&a, now), Some(Color::Red));
    }

    /// P0 fix: a `RateLimited` event from `StopFailure` carries no reset
    /// timestamp, but the row must still render the red cap badge —
    /// otherwise the user sees no visual indication that they're capped.
    #[test]
    fn limits_renders_red_cap_badge_when_scope_set_without_reset_time() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Error,
            None,
            None,
            None,
            None,
        );
        // Simulate StopFailure-only signal: scope set, until unknown.
        a.rate_limit_scope = Some(RateLimitScope::Unknown);
        a.rate_limited_until = None;
        let s = limits_cell_string(&a, now);
        assert!(s.starts_with("⛔ "), "expected cap glyph: {s:?}");
        assert_eq!(limits_cell_fg(&a, now), Some(Color::Red));
    }

    #[test]
    fn limits_ignores_expired_cap_and_falls_through_to_pct() {
        // A `rate_limited_until` in the past shouldn't drag the cell into
        // red — we want the row to recover to the utilisation view as soon
        // as the window rolls over (the daemon clears the field on the
        // next `Started`, but in-flight snapshots may still carry it).
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        );
        a.rate_limit_scope = Some(RateLimitScope::FiveHour);
        a.rate_limited_until = Some(now - time::Duration::minutes(1));
        a.rate_limit_5h_pct = Some(42.0);
        let s = limits_cell_string(&a, now);
        assert!(!s.contains('⛔'), "expected no cap glyph: {s:?}");
        assert!(s.contains("5h"), "expected utilisation text: {s:?}");
        // < 80% — default colour, not yellow.
        assert_eq!(limits_cell_fg(&a, now), None);
    }

    #[test]
    fn limits_renders_utilization_when_only_pct_is_set() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        );
        a.rate_limit_5h_pct = Some(84.0);
        assert_eq!(limits_cell_string(&a, now), "5h 84%");
        // ≥ 80 — yellow warning colour.
        assert_eq!(limits_cell_fg(&a, now), Some(Color::Yellow));
    }

    #[test]
    fn limits_picks_higher_pct_window_when_both_set() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        );
        a.rate_limit_5h_pct = Some(31.0);
        a.rate_limit_7d_pct = Some(72.0);
        // 7d wins (higher), and < 80 keeps the default colour.
        assert_eq!(limits_cell_string(&a, now), "7d 72%");
        assert_eq!(limits_cell_fg(&a, now), None);
    }

    #[test]
    fn limits_renders_dim_dash_when_nothing_is_set() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        );
        assert_eq!(limits_cell_string(&a, now), "-");
        assert_eq!(limits_cell_fg(&a, now), Some(Color::DarkGray));
    }

    #[test]
    fn limits_cap_badge_prefix_matches_scope() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        );
        a.rate_limited_until = Some(now + time::Duration::hours(1));

        a.rate_limit_scope = Some(RateLimitScope::SevenDay);
        assert!(
            limits_cell_string(&a, now).contains("7d "),
            "SevenDay should prefix with `7d `"
        );

        a.rate_limit_scope = Some(RateLimitScope::FiveHour);
        assert!(
            limits_cell_string(&a, now).contains("5h "),
            "FiveHour should prefix with `5h `"
        );

        // Unknown scope still renders the cap (scope is set), but
        // without a window-prefix string.
        a.rate_limit_scope = Some(RateLimitScope::Unknown);
        let s = limits_cell_string(&a, now);
        assert!(s.starts_with("⛔ "), "Unknown should still show cap: {s:?}");
        assert!(!s.contains("5h "), "Unknown should not prefix `5h `");
        assert!(!s.contains("7d "), "Unknown should not prefix `7d `");

        // No scope at all → no cap (the renderer's load-bearing gate).
        a.rate_limit_scope = None;
        let s = limits_cell_string(&a, now);
        assert!(!s.contains('⛔'), "None scope should drop the cap: {s:?}");
    }

    /// Capped without a reset timestamp — rendered with a bare scope or
    /// the literal "rate limited" suffix, no relative-time string.
    #[test]
    fn limits_cap_badge_without_reset_uses_capped_label() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Error,
            None,
            None,
            None,
            None,
        );
        a.rate_limit_scope = Some(RateLimitScope::FiveHour);
        a.rate_limited_until = None;
        assert_eq!(limits_cell_string(&a, now), "⛔ 5h capped");

        a.rate_limit_scope = Some(RateLimitScope::Unknown);
        assert_eq!(limits_cell_string(&a, now), "⛔ rate limited");
    }

    #[test]
    fn bare_pane_summary_lands_in_prompt_column() {
        // Render with a column set that includes Prompt but excludes the
        // others — we verify the BarePane row is built without panic and
        // that the prompt column carries the summary.
        let cfg = WatchConfig {
            view: WatchView::Pane,
            columns: vec!["pane".into(), "prompt".into()],
            widths: HashMap::new(),
            ..Default::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![],
            vec![fake_pane("%99", "main", 0, 0, "vim README.md")],
        );
        assert_eq!(app.rows.len(), 1);
        let backend = TestBackend::new(120, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // The pane current command should be visible somewhere in the row.
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("vim"),
            "expected pane summary in render: {text:?}"
        );
    }

    // ---- background refresh task -----------------------------------------

    fn outcome_with_marker(session: &str) -> RefreshOutcome {
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                session,
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            panes: vec![],
            sessions: vec![],
            session_activity: vec![],
            error: None,
        })
    }

    /// A wake request sent from the input side must trigger a refresh on
    /// the outcome channel. The fetcher counts calls and returns a marker
    /// outcome so the assertion can confirm it actually came from us.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wake_request_drives_a_refresh_outcome() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = Arc::clone(&calls);

        let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
        let (out_tx, mut out_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);

        let task = tokio::spawn(refresh_task(
            move || {
                let n = calls_for_fetch.fetch_add(1, Ordering::SeqCst);
                async move { outcome_with_marker(&format!("call-{n}")) }
            },
            wake_rx,
            out_tx,
            async { None },
        ));

        // The 500 ms tick is paused; force the wake path.
        wake_tx.try_send(()).expect("wake slot empty at start");
        let outcome = out_rx.recv().await.expect("refresh outcome on wake");
        let RefreshOutcome::Full(full) = outcome else {
            panic!("wake path always sends a Full refresh");
        };
        assert_eq!(full.agents.len(), 1);
        assert_eq!(full.agents[0].session_id, "call-0");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Dropping the wake sender ends the task cleanly.
        drop(wake_tx);
        task.await.expect("refresh_task joins on shutdown");
    }

    /// On the periodic tick (no wake), the fetcher still runs and an
    /// outcome lands on the channel. Validates that `MissedTickBehavior`
    /// + the consume-first-tick dance still produces deliveries.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn periodic_tick_drives_a_refresh_outcome() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = Arc::clone(&calls);

        let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
        let (out_tx, mut out_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);

        let task = tokio::spawn(refresh_task(
            move || {
                calls_for_fetch.fetch_add(1, Ordering::SeqCst);
                async { outcome_with_marker("tick") }
            },
            wake_rx,
            out_tx,
            async { None },
        ));

        // Time is paused; advance past one full POLL_INTERVAL so the
        // interval fires its second tick (the first is consumed inside
        // refresh_task before the loop).
        tokio::time::advance(POLL_INTERVAL + Duration::from_millis(50)).await;
        let outcome = out_rx.recv().await.expect("refresh outcome on tick");
        let RefreshOutcome::Full(full) = outcome else {
            panic!("periodic tick always sends a Full refresh");
        };
        assert_eq!(full.agents[0].session_id, "tick");
        assert!(calls.load(Ordering::SeqCst) >= 1);

        drop(wake_tx);
        task.await.expect("refresh_task joins on shutdown");
    }

    // ---- detail row -------------------------------------------------------

    #[test]
    fn detail_host_prefers_prompt_then_falls_back_to_last() {
        assert_eq!(
            detail_host_column(&[
                WatchColumn::Pane,
                WatchColumn::Prompt,
                WatchColumn::Activity
            ]),
            Some(1)
        );
        assert_eq!(
            detail_host_column(&[WatchColumn::Pane, WatchColumn::State, WatchColumn::Activity]),
            Some(2)
        );
        assert_eq!(detail_host_column(&[]), None);
    }

    #[test]
    fn format_detail_interpolates_known_vars_and_collapses_lines() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("first line\nsecond line"),
            Some("Opus"),
            Some(7.0),
            Some(0.05),
        );
        let row = WatchRow::agent(a);
        let s = format_detail("{model} · {last_prompt}", &row, &[], now).unwrap();
        assert!(s.contains("Opus"));
        assert!(s.contains("first line · second line"));
    }

    #[test]
    fn format_detail_resolves_last_response() {
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        );
        a.last_response = Some("here is what I did".into());
        let row = WatchRow::agent(a);
        let s = format_detail("{last_response}", &row, &[], now).unwrap();
        assert_eq!(s, "here is what I did");
    }

    /// The default detail template leads with `last_notification` so a
    /// blocked row answers "why does this need me" (the permission / choice
    /// text) at a glance, then falls back to response, then prompt.
    #[test]
    fn default_detail_template_prefers_notification_then_response_then_prompt() {
        let now = OffsetDateTime::now_utc();
        let template = &muxa::config::DetailConfig::default().template;

        // Notification present → wins.
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::WaitingInput,
            Some("run the migration?"),
            None,
            None,
            None,
        );
        a.last_response = Some("a response".into());
        a.last_notification = Some("approve permission to run rm -rf?".into());
        let row = WatchRow::agent(a);
        assert_eq!(
            format_detail(template, &row, &[], now).unwrap(),
            "approve permission to run rm -rf?"
        );

        // No notification → falls through to the response.
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("my prompt"),
            None,
            None,
            None,
        );
        a.last_response = Some("assistant reply".into());
        let row = WatchRow::agent(a);
        assert_eq!(
            format_detail(template, &row, &[], now).unwrap(),
            "assistant reply"
        );
    }

    #[test]
    fn format_detail_returns_none_when_only_dashes() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        );
        let row = WatchRow::agent(a);
        // Default template renders `—` because last_prompt is missing —
        // detail should be suppressed instead of cluttering the row.
        assert!(format_detail("{last_prompt}", &row, &[], now).is_none());
    }

    #[test]
    fn format_detail_falls_back_to_alternative_when_primary_is_dash() {
        // Pipe-separated alternatives should resolve left-to-right and
        // pick the first non-dash variable. This is the path the default
        // `{last_response|last_prompt}` template takes when an agent has
        // submitted a prompt but no `TurnStopped` response has landed —
        // covers the common "muxa watch right after typing" case.
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("review the diff"),
            None,
            None,
            None,
        );
        a.last_response = None;
        let row = WatchRow::agent(a);
        let s = format_detail("{last_response|last_prompt}", &row, &[], now).unwrap();
        assert_eq!(s, "review the diff");
    }

    #[test]
    fn format_detail_picks_primary_when_present_in_chain() {
        // When the first alternative resolves to a real value, later
        // alternatives are ignored — `last_response` wins over `last_prompt`.
        let now = OffsetDateTime::now_utc();
        let mut a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            Some("the prompt"),
            None,
            None,
            None,
        );
        a.last_response = Some("the response".into());
        let row = WatchRow::agent(a);
        let s = format_detail("{last_response|last_prompt}", &row, &[], now).unwrap();
        assert_eq!(s, "the response");
    }

    #[test]
    fn format_detail_chain_returns_dash_when_all_alternatives_empty() {
        // All alternatives produced placeholder dashes — outer
        // `format_detail` then suppresses the whole detail row, matching
        // the existing fresh-install behaviour. The chain helper itself
        // surfaces a dash (rather than `None`) so the placeholder isn't
        // mistaken for an unknown variable typo.
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        );
        let row = WatchRow::agent(a);
        assert!(format_detail("{last_response|last_prompt}", &row, &[], now).is_none());
    }

    #[test]
    fn format_detail_preserves_unknown_placeholder_literal() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hi"),
            None,
            None,
            None,
        );
        let row = WatchRow::agent(a);
        let s = format_detail("{nope} {last_prompt}", &row, &[], now).unwrap();
        assert!(s.contains("{nope}"));
        assert!(s.contains("hi"));
    }

    #[test]
    fn selected_row_renders_with_extra_detail_line() {
        // Render to a TestBackend and assert the detail prefix appears in
        // the buffer for the selected row only. Default template is
        // `{last_response}`, so the agent must have a captured response
        // for the detail line to render (otherwise it suppresses).
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        let mut a1 = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::WaitingInput,
            Some("waiting prompt that is long enough to be visible in the detail line"),
            None,
            None,
            None,
        );
        a1.last_response = Some("the assistant said something".into());
        let a2 = fake_agent(
            "s2",
            Some("%2"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            Some("another prompt"),
            None,
            None,
            None,
        );
        app.set_data(vec![a1, a2], vec![]);
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        // The ↳ marker is unique to the detail line.
        assert!(text.contains("↳"), "expected detail marker in render");
    }

    #[test]
    fn selected_row_keeps_configured_detail_alongside_workload() {
        let backend = TestBackend::new(110, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        let mut agent = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("prompt"),
            None,
            None,
            None,
        );
        agent.last_response = Some("assistant detail remains visible".into());
        agent.workload = muxa::WorkloadSummary {
            primary_pid: Some(20),
            process_count: 2,
            shell_count: 1,
            subagent_count: 0,
            helper_count: 0,
            preview: Vec::new(),
        };
        app.set_data(vec![agent], vec![]);

        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();

        assert!(text.contains("↳ tree ▸1 +1"), "missing tree detail: {text}");
        assert!(
            text.contains("assistant detail remains visible"),
            "configured detail must remain visible beside workload detail: {text:?}"
        );
    }

    #[test]
    fn detail_disabled_skips_expansion() {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            detail: muxa::config::DetailConfig {
                enabled: false,
                template: "{last_prompt}".into(),
            },
            ..Default::default()
        };
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                Some("hello"),
                None,
                None,
                None,
            )],
            vec![],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!text.contains("↳"), "detail line must be suppressed");
    }

    /// Mid-turn case: default config + an agent that has submitted a
    /// prompt but not yet captured a response. The default template's
    /// fallback (`{last_response|last_prompt}`) must surface the prompt
    /// on the detail line so the user sees what's currently in flight
    /// instead of an empty row.
    #[test]
    fn default_template_falls_back_to_last_prompt_when_no_response() {
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("a prompt the user just submitted"),
                None,
                None,
                None,
            )],
            vec![],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("↳"),
            "default template must show the prompt as a fallback when last_response is None"
        );
        assert!(
            text.contains("a prompt the user just submitted"),
            "the actual fallback content (last_prompt) must be visible in the rendered buffer"
        );
    }

    /// Truly-empty case: an agent with neither `last_response` nor
    /// `last_prompt`. Both alternatives in the default template resolve
    /// to dashes, the suppression rule kicks in, the row stays one line
    /// tall and no `↳` glyph appears. Preserves the fresh-install /
    /// freshly-discovered-pane behaviour from before the fallback fix.
    #[test]
    fn default_template_suppresses_detail_when_no_response_or_prompt() {
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            vec![],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !text.contains("↳"),
            "default template must suppress detail when both last_response and last_prompt are None"
        );
    }

    // ---- pane=None UX safety net ------------------------------------------

    #[test]
    fn footer_hints_when_selected_row_has_no_pane() {
        // Build a buffer-backed render with one agent that has no pane
        // (the SDK sub-agent case). Selecting it must put a yellow
        // "no tmux pane — attach unavailable" hint in the footer, where
        // a regular agent would just show the keybinds.
        //
        // Default config hides paneless agents — we explicitly include
        // them here because that's the very thing under test.
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let cfg = WatchConfig {
            view: WatchView::Pane,
            hide_paneless: false,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![fake_agent(
                "s-no-pane",
                None,
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("a sub-agent prompt"),
                None,
                None,
                None,
            )],
            vec![],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            dump.contains("attach unavailable"),
            "expected pane=None footer hint in render"
        );
        assert!(
            dump.contains("(no pane)"),
            "expected '(no pane)' label in PANE column"
        );
    }

    #[test]
    fn footer_hides_hint_when_selected_row_has_pane() {
        // Sanity check the inverse — a regular agent must NOT show the
        // pane=None hint.
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_data(
            vec![fake_agent(
                "s",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            vec![],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!dump.contains("attach unavailable"));
        assert!(!dump.contains("(no pane)"));
    }

    #[test]
    fn selected_pane_returns_none_for_no_pane_agent() {
        // include the paneless row under test
        let cfg = WatchConfig {
            view: WatchView::Pane,
            hide_paneless: false,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![fake_agent(
                "s",
                None,
                AgentKind::ClaudeCode,
                AgentState::Working,
                None,
                None,
                None,
                None,
            )],
            vec![],
        );
        // Table-mode Enter resolves through `selected_pane`; returning
        // None here is what lets the input handler surface a "no pane"
        // hint instead of opening the prompt composer.
        assert_eq!(app.selected_pane(), None);
    }

    /// Default config hides paneless agents so the picker only lists
    /// rows the user can actually attach to. The row count must reflect
    /// only the pane-bound agent, and `paneless_hidden` must record the
    /// count of filtered rows so the footer can surface them.
    #[test]
    fn hide_paneless_filters_agents_by_default() {
        let mut app = App::new();
        app.set_data(
            vec![
                fake_agent(
                    "with-pane",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "no-pane",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![],
        );
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.paneless_hidden, 1);
    }

    /// `--include-paneless` (or `[watch] hide_paneless = false`) keeps
    /// every agent visible and zeroes the filter counter.
    #[test]
    fn include_paneless_keeps_every_agent() {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            hide_paneless: false,
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent(
                    "with-pane",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "no-pane",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![],
        );
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.paneless_hidden, 0);
    }

    /// Hidden paneless agents that are blocked on a human are tallied into
    /// `paneless_attention` so the header can surface them — otherwise a
    /// detached agent that goes `WaitingInput` is invisible in the picker.
    /// Idle/working paneless agents count toward `paneless_hidden` but not
    /// `paneless_attention`.
    #[test]
    fn paneless_attention_counts_only_blocked_hidden_agents() {
        let mut app = App::new(); // default hides paneless
        app.set_data(
            vec![
                fake_agent(
                    "waiting-nopane",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::WaitingInput,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "error-nopane",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::Error,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "idle-nopane",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![],
        );
        assert_eq!(app.paneless_hidden, 3);
        assert_eq!(app.paneless_attention, 2);
    }

    /// Footer surfaces the hidden-paneless count so the rows aren't
    /// silently lost. The hint also tells the user how to reveal them.
    #[test]
    fn footer_shows_paneless_hidden_count() {
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(); // default hides paneless
        app.set_data(
            vec![
                fake_agent(
                    "with-pane",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "no-pane-1",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "no-pane-2",
                    None,
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            vec![],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            dump.contains("+2 paneless"),
            "footer must surface the hidden paneless count",
        );
        assert!(
            dump.contains("--include-paneless"),
            "footer must hint at the override flag",
        );
    }

    /// `apply_outcome` is the only path data flows back into `App`. It
    /// must mirror what the old inline `refresh` helper did: stash the
    /// error and feed agents+panes through `set_data`.
    #[test]
    fn apply_outcome_mirrors_old_refresh_helper() {
        let mut app = App::new();
        let outcome = RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            panes: vec![fake_pane("%99", "side", 0, 0, "vim")],
            sessions: vec![],
            session_activity: vec![],
            error: Some(DaemonError {
                self_describing: false,
                message: "boom".into(),
            }),
        });
        apply_outcome(&mut app, outcome);
        assert_eq!(app.rows.len(), 2);
        assert!(app.last_error.is_some());
        assert_eq!(app.last_error.as_ref().unwrap().message, "boom");
    }

    /// Regression for the "all rows flicker through Starting" report on
    /// v0.5.0. The push-based `Subscribe` stream triggers a fresh
    /// snapshot per transition; if any of those snapshots momentarily
    /// returns a row in `Starting` (e.g. because a new entry was just
    /// inserted by an event that didn't carry an explicit transition),
    /// the row would visibly flicker cyan for one tick before settling
    /// back. `apply_outcome` MUST keep the previously-known steady
    /// state for an `(kind, session_id)` already in `app.rows`.
    #[test]
    fn apply_outcome_preserves_state_on_unchanged_rows() {
        let mut app = App::new();

        // First refresh: row %1 lands in `Working`.
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Working,
                    None,
                    None,
                    None,
                    None,
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );
        assert_eq!(app.rows.len(), 1);
        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        assert_eq!(a.state, AgentState::Working);

        // Second refresh: same `(kind, session_id)`, but state has
        // regressed to `Starting` — the daemon-side bug we're papering
        // over here. `apply_outcome` must NOT propagate that to the
        // table; the user sees `Working` continuously.
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Starting,
                    None,
                    None,
                    None,
                    None,
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );
        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        assert_eq!(
            a.state,
            AgentState::Working,
            "row that was Working must not flicker through Starting on a transient placeholder snapshot",
        );

        // Third refresh: legitimate transition Working → Idle. The
        // merge MUST let real state changes through — the invariant
        // is "Starting placeholder doesn't override steady state",
        // not "state never changes".
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    None,
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );
        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        assert_eq!(a.state, AgentState::Idle);
    }

    /// A genuinely fresh agent (no prior row at the same
    /// `(kind, session_id)`) is allowed to appear as `Starting` — the
    /// merge only suppresses the `Starting` placeholder when we have
    /// a steady state to fall back on.
    #[test]
    fn apply_outcome_lets_starting_through_for_brand_new_rows() {
        let mut app = App::new();
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Starting,
                    None,
                    None,
                    None,
                    None,
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );
        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        assert_eq!(a.state, AgentState::Starting);
    }

    /// `SingleAgent` push only touches the matching `(kind, session_id)`
    /// row. Other rows keep their existing values byte-for-byte —
    /// this is what stops the UI from "redrawing every row on every
    /// tick".
    #[test]
    fn single_agent_outcome_only_updates_matching_row() {
        let mut app = App::new();

        // Seed two agent rows.
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![
                    fake_agent(
                        "s1",
                        Some("%1"),
                        AgentKind::ClaudeCode,
                        AgentState::Working,
                        Some("first prompt"),
                        None,
                        None,
                        None,
                    ),
                    fake_agent(
                        "s2",
                        Some("%2"),
                        AgentKind::ClaudeCode,
                        AgentState::Idle,
                        Some("second prompt"),
                        None,
                        None,
                        None,
                    ),
                ],
                panes: vec![
                    fake_pane("%1", "main", 0, 0, "claude"),
                    fake_pane("%2", "side", 1, 0, "claude"),
                ],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );
        assert_eq!(app.rows.len(), 2);

        // SingleAgent push for s2 only — last_prompt and state change.
        apply_outcome(
            &mut app,
            RefreshOutcome::SingleAgent(fake_agent(
                "s2",
                Some("%2"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("second prompt UPDATED"),
                None,
                None,
                None,
            )),
        );

        // s1 must be untouched.
        let WatchRow::Agent(a1) = &app.rows[0] else {
            panic!("row 0 should be agent s1");
        };
        assert_eq!(a1.session_id, "s1");
        assert_eq!(a1.state, AgentState::Working);
        assert_eq!(a1.last_prompt.as_deref(), Some("first prompt"));

        // s2 reflects the push.
        let WatchRow::Agent(a2) = &app.rows[1] else {
            panic!("row 1 should be agent s2");
        };
        assert_eq!(a2.session_id, "s2");
        assert_eq!(a2.state, AgentState::Working);
        assert_eq!(a2.last_prompt.as_deref(), Some("second prompt UPDATED"));
    }

    /// `SingleAgent` push for an unknown `(kind, session_id)` appends.
    /// The next periodic Full refresh handles sort order.
    #[test]
    fn single_agent_outcome_appends_unknown_row() {
        let mut app = App::new();
        apply_outcome(
            &mut app,
            RefreshOutcome::SingleAgent(fake_agent(
                "fresh",
                Some("%9"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )),
        );
        assert_eq!(app.rows.len(), 1);
        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        assert_eq!(a.session_id, "fresh");
    }

    /// A `SingleAgent` push whose payload carries `last_prompt: None`
    /// must not wipe the prompt the user already sees. This happens
    /// when the daemon's broadcast captures a transition that didn't
    /// touch the prompt field (e.g. `ToolStarted` → `Working`).
    #[test]
    fn single_agent_preserves_last_prompt_when_none() {
        let mut app = App::new();
        // Seed with a row that has a prompt.
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Working,
                    Some("original prompt"),
                    Some("Opus"),
                    Some(34.0),
                    Some(0.12),
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );

        // Push a state-only update (no prompt).
        apply_outcome(
            &mut app,
            RefreshOutcome::SingleAgent(fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None, // last_prompt cleared in payload
                None,
                None,
                None,
            )),
        );

        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        assert_eq!(a.state, AgentState::Idle);
        assert_eq!(a.last_prompt.as_deref(), Some("original prompt"));
        // Other optional fields should also survive.
        assert_eq!(a.model.as_deref(), Some("Opus"));
        assert_eq!(a.context_used_pct, Some(34.0));
        assert_eq!(a.cost_usd, Some(0.12));
    }

    /// A `Full` refresh that carries a `Starting` placeholder with
    /// `last_prompt: None` must not blank the prompt we already know.
    /// This is the fallback-tick path (5 s interval) hitting a daemon
    /// that has a fresh store entry for a pane we've been tracking.
    #[test]
    fn apply_full_preserves_last_prompt_on_starting_placeholder() {
        let mut app = App::new();
        // First refresh: row is Working with a prompt.
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Working,
                    Some("original prompt"),
                    Some("Sonnet"),
                    Some(50.0),
                    Some(0.05),
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );

        // Second refresh: snapshot regressed to Starting + blank optional fields.
        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Starting,
                    None,
                    None,
                    None,
                    None,
                )],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );

        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        // Anti-flicker: state should stay Working.
        assert_eq!(a.state, AgentState::Working);
        // Anti-data-loss: prompt and metadata must survive.
        assert_eq!(a.last_prompt.as_deref(), Some("original prompt"));
        assert_eq!(a.model.as_deref(), Some("Sonnet"));
        assert_eq!(a.context_used_pct, Some(50.0));
        assert_eq!(a.cost_usd, Some(0.05));
    }

    /// A `SingleAgent` push whose payload carries cleared
    /// rate-limit fields (`rate_limited_until`, `rate_limit_scope`,
    /// `rate_limit_source`) must propagate the clear to the UI. These
    /// three fields are NOT merge-preserved because events like
    /// `Started` and `TurnStopped` legitimately clear them — a new
    /// session or a successful turn means the cap has been lifted.
    #[test]
    fn single_agent_propagates_rate_limit_clear() {
        use muxa::event::{RateLimitScope, RateLimitSource};
        let mut app = App::new();

        // Seed a row that is currently rate-limited.
        let mut limited = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Error,
            Some("prompt"),
            Some("Opus"),
            None,
            None,
        );
        limited.rate_limited_until = Some(time::macros::datetime!(2026-05-06 15:00:00 UTC));
        limited.rate_limit_scope = Some(RateLimitScope::FiveHour);
        limited.rate_limit_source = Some(RateLimitSource::StopFailure);
        limited.rate_limit_5h_pct = Some(100.0);

        apply_outcome(
            &mut app,
            RefreshOutcome::Full(FullRefresh {
                agents: vec![limited],
                panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
                sessions: vec![],
                session_activity: vec![],
                error: None,
            }),
        );
        assert_eq!(app.rows.len(), 1);

        // Push a transition that clears the cap (e.g. TurnStopped after
        // a successful turn, or Started for a fresh session).
        apply_outcome(
            &mut app,
            RefreshOutcome::SingleAgent(fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None, // last_prompt should be preserved
                None, // model should be preserved
                None,
                None,
            )),
        );

        let WatchRow::Agent(a) = &app.rows[0] else {
            panic!("expected agent row");
        };
        // Rate-limit markers must be gone.
        assert!(
            a.rate_limited_until.is_none(),
            "rate_limited_until must be cleared"
        );
        assert!(
            a.rate_limit_scope.is_none(),
            "rate_limit_scope must be cleared"
        );
        assert!(
            a.rate_limit_source.is_none(),
            "rate_limit_source must be cleared"
        );
        // But last_prompt and model should survive (None means "don't touch").
        assert_eq!(a.last_prompt.as_deref(), Some("prompt"));
        assert_eq!(a.model.as_deref(), Some("Opus"));
        // Rolling percentages should also survive (they're not cleared by events).
        assert_eq!(a.rate_limit_5h_pct, Some(100.0));
    }

    // ---- detail row: precise visual layout --------------------------------

    /// Read one full visual row of text from the `TestBackend` buffer. Trims
    /// trailing whitespace so callers can substring-match without juggling
    /// padding.
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        let area = buf.area();
        let mut s = String::with_capacity(usize::from(area.width));
        for x in 0..area.width {
            s.push_str(buf.cell((x, y)).map_or("", ratatui::buffer::Cell::symbol));
        }
        s.trim_end().to_string()
    }

    /// Build an `App` configured to put the detail line on the Prompt
    /// column with three rows of agents. Returns the constructed app.
    fn three_agent_app(detail: muxa::config::DetailConfig) -> App {
        // Pin sort to PaneId so the assertions about which row is at
        // which index don't drift with the default [Session, Activity]
        // sort once `fake_agent` timestamps differ across runs.
        let cfg = WatchConfig {
            view: WatchView::Pane,
            detail,
            sort: vec![WatchSortKey::PaneId],
            ..Default::default()
        };
        let mut app = App::with_config(cfg);
        seed_three_agents(&mut app);
        app
    }

    /// Drop the canonical three-agent fixture into an arbitrary App —
    /// lets tests pick their own `WatchConfig` (e.g. to pin
    /// `[watch.preview] default_content`) and then reuse the same
    /// agent set the rest of the suite expects.
    fn seed_three_agents(app: &mut App) {
        let mut a1 = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("ALPHAprompt"),
            Some("Opus"),
            Some(7.0),
            Some(0.05),
        );
        a1.last_response = Some("ALPHAresp".into());
        let mut a2 = fake_agent(
            "s2",
            Some("%2"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            Some("BETAprompt"),
            None,
            None,
            None,
        );
        a2.last_response = Some("BETAresp".into());
        let mut a3 = fake_agent(
            "s3",
            Some("%3"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("GAMMAprompt"),
            None,
            None,
            None,
        );
        a3.last_response = Some("GAMMAresp".into());
        app.set_data(vec![a1, a2, a3], vec![]);
    }

    /// Selected row's host column should render exactly 2 visual lines:
    /// the original cell on row N, the `↳ <detail>` hint on row N+1.
    /// Non-selected rows must remain 1 line tall and no detail glyph
    /// should appear on their row.
    #[test]
    fn detail_line_lands_on_row_below_selection() {
        let backend = TestBackend::new(140, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        // Select the middle row so we can probe rows on both sides.
        app.table_state.select(Some(1));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        // Layout: header chunk = 3 rows (y=0..2), then table chunk with
        // `Borders::ALL` + a header row -> data rows start at y = 3 + 1
        // (top border) + 1 (header) = y=5.
        // Rows:
        //   y=5  ALPHAprompt (row 0, not selected)
        //   y=6  BETAprompt  (row 1, selected, 2-line)
        //   y=7  ↳ BETAresp  (detail line — default template is `{last_response}`)
        //   y=8  GAMMAprompt (row 2, not selected)
        let r0 = row_text(buf, 5);
        let r1 = row_text(buf, 6);
        let r1_detail = row_text(buf, 7);
        let r2 = row_text(buf, 8);

        assert!(r0.contains("ALPHAprompt"), "row 0 missing top text: {r0:?}");
        assert!(!r0.contains("↳"), "row 0 must not carry detail: {r0:?}");
        assert!(
            r1.contains("BETAprompt"),
            "selected row missing top text: {r1:?}"
        );
        assert!(
            !r1.contains("↳"),
            "selected row's first line must not be the detail line: {r1:?}"
        );
        assert!(
            r1_detail.contains("↳") && r1_detail.contains("BETAresp"),
            "detail line not on the row directly below the selection: {r1_detail:?}"
        );
        assert!(r2.contains("GAMMAprompt"), "row 2 missing top text: {r2:?}");
        assert!(!r2.contains("↳"), "row 2 must not carry detail: {r2:?}");
    }

    /// When `[watch.detail] enabled = false`, every row should be one
    /// visual line — the rows pack against each other with no gap, and
    /// no `↳` glyph appears anywhere.
    #[test]
    fn detail_disabled_keeps_all_rows_at_one_line() {
        let backend = TestBackend::new(140, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig {
            enabled: false,
            template: "{last_prompt}".into(),
        });
        app.table_state.select(Some(1));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // y=5/6/7 should be the three packed rows.
        assert!(row_text(buf, 5).contains("ALPHAprompt"));
        assert!(row_text(buf, 6).contains("BETAprompt"));
        assert!(row_text(buf, 7).contains("GAMMAprompt"));
        // No detail anywhere.
        let dump: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!dump.contains("↳"));
    }

    /// `format_detail` output must round-trip through render: even a
    /// custom template (referencing a non-prompt var) shows up under
    /// the selected row.
    #[test]
    fn custom_template_renders_below_selection() {
        let detail = muxa::config::DetailConfig {
            enabled: true,
            template: "model={model} prompt={last_prompt}".into(),
        };
        let backend = TestBackend::new(160, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(detail);
        app.table_state.select(Some(0));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Selected row (idx 0) sits at y=5; detail at y=6.
        let detail_line = row_text(buf, 6);
        assert!(
            detail_line.contains("↳"),
            "expected detail glyph at y=6: {detail_line:?}"
        );
        assert!(
            detail_line.contains("model=Opus"),
            "expected interpolated model: {detail_line:?}"
        );
        assert!(
            detail_line.contains("prompt=ALPHAprompt"),
            "expected interpolated prompt: {detail_line:?}"
        );
    }

    // ---- detail edge cases -----------------------------------------------

    #[test]
    fn empty_template_returns_none() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hi"),
            None,
            None,
            None,
        );
        let row = WatchRow::agent(a);
        // Empty after trimming -> suppress.
        assert!(format_detail("", &row, &[], now).is_none());
        assert!(format_detail("   ", &row, &[], now).is_none());
    }

    #[test]
    fn literal_only_template_passes_through_verbatim() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hi"),
            None,
            None,
            None,
        );
        let row = WatchRow::agent(a);
        let s = format_detail("just a literal string", &row, &[], now).unwrap();
        assert_eq!(s, "just a literal string");
    }

    #[test]
    fn unmatched_open_brace_does_not_panic() {
        let now = OffsetDateTime::now_utc();
        let a = fake_agent(
            "s",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hi"),
            None,
            None,
            None,
        );
        let row = WatchRow::agent(a);
        // Trailing `{last_prompt` (no closing `}`) — current behavior
        // emits the literal so the user sees their typo.
        let s = format_detail("oops {last_prompt", &row, &[], now).unwrap();
        assert!(s.contains("oops"));
        assert!(s.contains("{last_prompt"));
    }

    #[test]
    fn truncate_chars_boundary_at_max() {
        // Exactly `max` -> no ellipsis.
        let s_240: String = "a".repeat(240);
        let out = truncate_chars(&s_240, 240);
        assert_eq!(out.chars().count(), 240);
        assert!(!out.ends_with('…'));

        // 241 -> ellipsis appended after 240 chars.
        let s_241: String = "a".repeat(241);
        let out = truncate_chars(&s_241, 240);
        assert_eq!(out.chars().count(), 241);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_chars_handles_multibyte_and_wide() {
        // 5 wide CJK chars, max 3 -> 3 chars + ellipsis.
        let out = truncate_chars("가나다라마", 3);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("가나다"));

        // Emoji (single scalar) — no panic on byte boundaries.
        let out2 = truncate_chars("🦀🦀🦀🦀", 2);
        assert_eq!(out2.chars().count(), 3);
    }

    #[test]
    fn very_long_detail_renders_without_panic() {
        // Long enough to exceed any reasonable terminal width plus the
        // `truncate_chars` ceiling. Default template is `{last_response}`,
        // so the long string lives on the response field.
        let long: String = "x".repeat(2000);
        let cfg = WatchConfig::default();
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::with_config(cfg);
        let mut a = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hi"),
            None,
            None,
            None,
        );
        a.last_response = Some(long);
        app.set_data(vec![a], vec![]);
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // The detail line is capped at 240 chars + ellipsis; we only
        // care that a `↳` glyph still got placed and rendering didn't
        // wrap-around or panic.
        let dump: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(dump.contains("↳"));
    }

    #[test]
    fn bare_pane_detail_resolves_dashes_for_agent_only_vars() {
        // Custom template references `{model}` (agent-only). For a bare
        // pane this should resolve to `—`. With ONLY agent-only vars,
        // the suppress-empty rule does not kick in (since the template
        // has a literal "model=" prefix), so we should still get a
        // detail line — and the rendered content includes the dash.
        let now = OffsetDateTime::now_utc();
        let p = fake_pane("%99", "side", 0, 0, "vim");
        let row = WatchRow::BarePane(Box::new(p));
        let s = format_detail("model={model}", &row, &[], now).unwrap();
        assert_eq!(s, "model=—");
    }

    #[test]
    fn bare_pane_detail_with_only_agent_var_collapses_to_dash() {
        // Template that resolves *purely* to `—` after interpolation —
        // current `format_detail` suppresses this so we don't render
        // a detail line that says nothing useful.
        let now = OffsetDateTime::now_utc();
        let p = fake_pane("%99", "side", 0, 0, "vim");
        let row = WatchRow::BarePane(Box::new(p));
        assert!(format_detail("{model}", &row, &[], now).is_none());
    }

    #[test]
    fn selection_at_last_row_with_short_terminal_does_not_panic() {
        // 8 rows tall: header(3) + table(top border+header+rows...) + footer(1).
        // With 3 agents and the last selected (height=2), the table
        // body needs ~5 rows of content. Terminal is intentionally
        // tight to surface any clipping panics from ratatui.
        let backend = TestBackend::new(120, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(2));
        // Render twice in case the first frame computes a different
        // viewport offset than the second (TableState retains state).
        terminal.draw(|f| render(f, &mut app)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
    }

    // ---- preview overlay (key `p`) ----------------------------------------

    /// Drive `handle_event` with a single `Char(c)` keystroke and return
    /// the resulting Action. Centralises the boilerplate so each preview
    /// test reads as "press X, expect Y".
    fn key_action(app: &mut App, c: char) -> Action {
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            app,
        )
    }

    fn alt_key_action(app: &mut App, c: char) -> Action {
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)),
            app,
        )
    }

    /// Press a key and apply the resulting `Action` to `app` the same
    /// way the main run loop does. Mirrors the dispatch table in
    /// `watch::run` so tests can read as "press X, expect Y" without
    /// inlining the open/close/toggle book-keeping every time.
    fn press(app: &mut App, c: char) {
        let action = if app.preview.is_none() && c.eq_ignore_ascii_case(&'p') {
            alt_key_action(app, c)
        } else {
            key_action(app, c)
        };
        match action {
            Action::OpenPreview => {
                if let Some(pane) = app.selected_pane() {
                    app.preview = Some(PreviewState {
                        pane_id: pane,
                        scroll: 0,
                        mode: PreviewMode::Popup,
                        content: app.watch_cfg.preview.default_content,
                    });
                }
            }
            Action::ClosePreview => {
                app.preview = None;
                app.pane_capture = None;
            }
            Action::TogglePreviewMode => {
                if let Some(p) = app.preview.as_mut() {
                    p.mode = match p.mode {
                        PreviewMode::Popup => PreviewMode::Fullscreen,
                        PreviewMode::Fullscreen => PreviewMode::Popup,
                    };
                }
            }
            Action::TogglePreviewContent => {
                if let Some(p) = app.preview.as_mut() {
                    p.content = match p.content {
                        PreviewContent::PromptResponse => PreviewContent::LivePane,
                        PreviewContent::LivePane => PreviewContent::PromptResponse,
                    };
                    p.scroll = 0;
                    if matches!(p.content, PreviewContent::PromptResponse) {
                        app.pane_capture = None;
                    }
                }
            }
            Action::SetSort(preset) => {
                app.apply_sort_preset(preset);
            }
            Action::SetView(view) => {
                app.apply_view(view);
            }
            // Quick-action paths aren't exercised through `press` —
            // tests that need them call `handle_event` directly so
            // they can inspect the `Action` variant. Anything we
            // encounter here is treated as "no-op" for the existing
            // preview-suite assertions.
            Action::None
            | Action::Quit
            | Action::Refresh
            | Action::AttachPane(_)
            | Action::OpenCollaborationMessage
            | Action::OpenCollaborationMailbox
            | Action::SubmitCollaboration
            | Action::CancelCollaborationComposer
            | Action::ClaimCollaborationInbox
            | Action::AskConfirm(_)
            | Action::ConfirmYes
            | Action::ConfirmCancel
            | Action::Quick(_)
            | Action::NotApplicable(_) => {}
        }
    }

    #[test]
    fn preview_opens_with_alt_p_and_pins_selected_pane_id() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(1)); // %2

        assert!(app.preview.is_none());
        let action = alt_key_action(&mut app, 'p');
        assert!(matches!(action, Action::OpenPreview));

        // run loop applies OpenPreview by reading selected_pane(); we
        // inline the same effect here.
        if let Action::OpenPreview = action {
            if let Some(pane) = app.selected_pane() {
                app.preview = Some(PreviewState {
                    pane_id: pane,
                    scroll: 0,
                    mode: PreviewMode::Popup,
                    content: PreviewContent::PromptResponse,
                });
            }
        }
        assert_eq!(
            app.preview.as_ref().map(|p| p.pane_id.as_str()),
            Some("%2"),
            "preview should pin the pane id of the selected row"
        );
    }

    #[test]
    fn preview_closes_with_q_esc_p_or_o() {
        for key in [
            KeyCode::Char('q'),
            KeyCode::Esc,
            KeyCode::Char('p'),
            KeyCode::Char('o'),
        ] {
            let mut app = three_agent_app(muxa::config::DetailConfig::default());
            app.preview = Some(PreviewState {
                pane_id: "%1".into(),
                scroll: 0,
                mode: PreviewMode::Popup,
                content: PreviewContent::PromptResponse,
            });
            let action = handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app);
            assert!(
                matches!(action, Action::ClosePreview),
                "key {key:?} must request ClosePreview while in preview mode"
            );
        }
    }

    #[test]
    fn preview_enter_opens_prompt_for_pinned_pane() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.panes = vec![fake_pane("%1", "alpha", 0, 0, "claude")];
        // Select a different table row to prove Enter targets the
        // preview-pinned pane, not whatever the table cursor says now.
        app.table_state.select(Some(2));
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });

        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(
            matches!(action, Action::AttachPane(ref pane) if pane == "%1"),
            "preview Enter must attach to the pinned pane, got {action:?}"
        );
    }

    #[test]
    fn preview_arrow_keys_scroll_instead_of_moving_table_cursor() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(0));
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });

        // j scrolls down by 1
        let _ = key_action(&mut app, 'j');
        assert_eq!(app.preview.as_ref().unwrap().scroll, 1);
        // PageDown jumps by 10
        let _ = handle_event(
            Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            &mut app,
        );
        assert_eq!(app.preview.as_ref().unwrap().scroll, 11);
        // Home returns to top
        let _ = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            &mut app,
        );
        assert_eq!(app.preview.as_ref().unwrap().scroll, 0);
        // k saturates at 0 — must not underflow
        let _ = key_action(&mut app, 'k');
        assert_eq!(app.preview.as_ref().unwrap().scroll, 0);
        // Table cursor must NOT have moved during any of the above —
        // arrow keys belong to the preview while it's open.
        assert_eq!(app.table_state.selected(), Some(0));
    }

    fn session_preview_app() -> App {
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            preview: muxa::config::PreviewConfig {
                default_content: PreviewContent::PromptResponse,
            },
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let mut alpha = fake_agent_at("alpha", "%1", t0);
        alpha.last_prompt = Some("ALPHAprompt".into());
        alpha.last_response = Some("ALPHAresp".into());
        let mut beta = fake_agent_at("beta", "%2", t1);
        beta.last_prompt = Some("BETAprompt".into());
        beta.last_response = Some("BETAresp".into());
        app.set_data_with_sessions(
            vec![alpha, beta],
            vec![
                fake_pane("%1", "main", 0, 0, "claude"),
                fake_pane("%2", "main", 0, 1, "codex"),
            ],
            vec![fake_session("$1", "main", 1)],
            vec![],
        );
        app.table_state.select(Some(0));
        app
    }

    #[test]
    fn session_preview_keeps_all_agent_panes_available() {
        let app = session_preview_app();
        let WatchRow::Session(row) = &app.rows[0] else {
            panic!("expected session row");
        };
        assert_eq!(row.representative_pane.as_deref(), Some("%2"));
        assert_eq!(row.agents.len(), 2);
        assert_eq!(
            session_preview_targets(row),
            vec!["%1".to_string(), "%2".to_string()]
        );
        assert_eq!(preview_target_position(&app, "%2"), Some((2, 2)));
    }

    #[test]
    fn preview_brackets_cycle_session_agents_and_reset_cache() {
        let mut app = session_preview_app();
        app.preview = Some(PreviewState {
            pane_id: "%2".into(),
            scroll: 7,
            mode: PreviewMode::Popup,
            content: PreviewContent::LivePane,
        });
        app.pane_capture = Some(CapturedPane {
            pane_id: "%2".into(),
            text: "old screen".into(),
            fetched_at: std::time::Instant::now(),
        });

        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::None));
        let preview = app.preview.as_ref().unwrap();
        assert_eq!(
            preview.pane_id, "%1",
            "] should wrap to the first agent pane"
        );
        assert_eq!(preview.scroll, 0, "agent switch must reset scroll");
        assert!(
            app.pane_capture.is_none(),
            "agent switch must invalidate live capture cache",
        );

        let _ = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
            &mut app,
        );
        assert_eq!(
            app.preview.as_ref().map(|p| p.pane_id.as_str()),
            Some("%2"),
            "[ should wrap back to the previous agent pane",
        );
    }

    #[test]
    fn preview_lines_show_non_representative_session_agent() {
        let app = session_preview_app();
        let lines = build_preview_lines(&app, "%1");
        let dump = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(dump.contains("ALPHAprompt"), "missing non-latest prompt");
        assert!(dump.contains("ALPHAresp"), "missing non-latest response");
        assert!(
            !dump.contains("BETAprompt"),
            "preview leaked the session representative agent",
        );
    }

    #[test]
    fn preview_lines_show_prompt_response_and_notification_for_active_agent() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        // a1 was built with prompt "ALPHAprompt" + response "ALPHAresp".
        let lines = build_preview_lines(&app, "%1");
        let dump = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(dump.contains("Last prompt:"), "missing prompt heading");
        assert!(dump.contains("ALPHAprompt"), "missing prompt body");
        assert!(dump.contains("Last response:"), "missing response heading");
        assert!(dump.contains("ALPHAresp"), "missing response body");
        // Sanity-check that other agents' content didn't leak into the
        // preview for %1 — pane pinning must isolate.
        assert!(
            !dump.contains("BETAprompt"),
            "preview leaked content from a different agent"
        );

        // With a notification set, that section appears too. Force it on
        // the matching row and re-render.
        if let WatchRow::Agent(a) = &mut app.rows[0] {
            a.last_notification = Some("ready".into());
        }
        let dump2 = build_preview_lines(&app, "%1")
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump2.contains("Last notification:"));
        assert!(dump2.contains("ready"));
    }

    #[test]
    fn preview_for_unknown_pane_renders_fallback_message() {
        let app = three_agent_app(muxa::config::DetailConfig::default());
        let lines = build_preview_lines(&app, "%does-not-exist");
        let dump = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            dump.contains("no agent record"),
            "expected fallback hint, got: {dump}"
        );
    }

    #[test]
    fn preview_render_does_not_panic() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // Footer must show the preview-mode hints, not the table-mode
        // ones — `attach` is only meaningful with a row selected.
        let buf = terminal.backend().buffer();
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("scroll"),
            "preview footer must show scroll hint"
        );
        assert!(text.contains("back"), "preview footer must show back hint");
        assert!(
            !text.contains("attach"),
            "preview footer must not show attach hint"
        );
    }

    #[test]
    fn preview_defaults_to_popup_mode_when_opened() {
        // Pressing `p` from the table opens the overlay as a centred
        // popup, not full-screen — keeps surrounding rows visible.
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(0));
        let action = alt_key_action(&mut app, 'p');
        assert!(matches!(action, Action::OpenPreview));
        if let (Action::OpenPreview, Some(pane)) = (action, app.selected_pane()) {
            app.preview = Some(PreviewState {
                pane_id: pane,
                scroll: 0,
                mode: PreviewMode::Popup,
                content: PreviewContent::PromptResponse,
            });
        }
        assert_eq!(
            app.preview.as_ref().map(|p| p.mode),
            Some(PreviewMode::Popup)
        );
    }

    #[test]
    fn preview_f_toggles_between_popup_and_fullscreen() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });

        // First `f` requests TogglePreviewMode; the run loop applies the
        // flip. We mirror that here so the test asserts the end state.
        let action = key_action(&mut app, 'f');
        assert!(matches!(action, Action::TogglePreviewMode));
        if let Some(p) = app.preview.as_mut() {
            p.mode = match p.mode {
                PreviewMode::Popup => PreviewMode::Fullscreen,
                PreviewMode::Fullscreen => PreviewMode::Popup,
            };
        }
        assert_eq!(
            app.preview.as_ref().map(|p| p.mode),
            Some(PreviewMode::Fullscreen),
            "first `f` from popup must enter fullscreen"
        );

        // And `f` again flips back.
        let action = key_action(&mut app, 'f');
        if let (Action::TogglePreviewMode, Some(p)) = (action, app.preview.as_mut()) {
            p.mode = match p.mode {
                PreviewMode::Popup => PreviewMode::Fullscreen,
                PreviewMode::Fullscreen => PreviewMode::Popup,
            };
        }
        assert_eq!(
            app.preview.as_ref().map(|p| p.mode),
            Some(PreviewMode::Popup),
            "second `f` from fullscreen must return to popup"
        );
    }

    #[test]
    fn fullscreen_preview_render_does_not_panic_and_hides_table() {
        // The takeover path: in Fullscreen mode the table is replaced
        // wholesale. We can't easily assert the table is *gone* (the
        // header still mentions agent counts), but rendering must not
        // panic and the preview footer must show the popup-toggle hint.
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Fullscreen,
            content: PreviewContent::PromptResponse,
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let text: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("popup"),
            "fullscreen footer must offer popup toggle"
        );
        assert!(text.contains("Last prompt"));
        assert!(text.contains("Last response"));
    }

    #[test]
    fn centered_rect_returns_inner_box_within_parent() {
        // Sanity-check the popup geometry so downstream code can trust
        // the popup never escapes its parent.
        let parent = Rect::new(0, 0, 100, 30);
        let inner = centered_rect(80, 70, parent);

        assert!(inner.x >= parent.x);
        assert!(inner.y >= parent.y);
        assert!(inner.x + inner.width <= parent.x + parent.width);
        assert!(inner.y + inner.height <= parent.y + parent.height);
        // Roughly centred — left margin matches right margin within 1
        // cell to absorb integer-percentage rounding.
        let left = inner.x - parent.x;
        let right = (parent.x + parent.width) - (inner.x + inner.width);
        assert!(
            left.abs_diff(right) <= 1,
            "popup must be horizontally centred (left={left}, right={right})"
        );
    }

    #[test]
    fn bottom_prompt_rect_hugs_parent_bottom() {
        let parent = Rect::new(2, 3, 100, 10);
        assert_eq!(bottom_prompt_rect(parent), Rect::new(2, 10, 100, 3));

        let short = Rect::new(2, 3, 100, 2);
        assert_eq!(bottom_prompt_rect(short), short);
    }

    /// `c` toggles the preview content axis: `PromptResponse` → `LivePane`
    /// → `PromptResponse`. Geometry mode (popup vs fullscreen) is unaffected
    /// — the two axes compose. Scroll resets so the new content surface
    /// starts at the top instead of mid-line.
    /// Overlay preset that opens to `PromptResponse` — used by tests that
    /// want to pin the starting content axis instead of inheriting whatever
    /// the global default happens to be. Keeps test intent stable across
    /// future default flips.
    fn cfg_with_prompt_default() -> WatchConfig {
        WatchConfig {
            view: WatchView::Pane,
            preview: muxa::config::PreviewConfig {
                default_content: PreviewContent::PromptResponse,
            },
            ..WatchConfig::default()
        }
    }

    #[test]
    fn c_toggles_preview_content_and_resets_scroll() {
        let mut app = App::with_config(cfg_with_prompt_default());
        seed_three_agents(&mut app);
        app.table_state.select(Some(0));
        // Open the preview and scroll into the body.
        press(&mut app, 'p');
        app.preview.as_mut().unwrap().scroll = 7;
        assert_eq!(
            app.preview.as_ref().unwrap().content,
            PreviewContent::PromptResponse,
        );

        press(&mut app, 'c');
        let p = app.preview.as_ref().unwrap();
        assert_eq!(p.content, PreviewContent::LivePane);
        assert_eq!(p.scroll, 0, "content toggle must reset scroll");
        assert_eq!(p.mode, PreviewMode::Popup, "geometry must not flip");

        press(&mut app, 'c');
        assert_eq!(
            app.preview.as_ref().unwrap().content,
            PreviewContent::PromptResponse,
            "second `c` must flip back to prompt/response",
        );
    }

    /// Default `WatchConfig` opens the overlay in `LivePane` mode — this
    /// is the headline UX change the `[watch.preview] default_content`
    /// option ships with. A future flip of the default would land here
    /// loud and clear.
    #[test]
    fn default_config_opens_preview_in_live_pane() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(0));
        press(&mut app, 'p');
        let p = app.preview.as_ref().unwrap();
        assert_eq!(
            p.content,
            PreviewContent::LivePane,
            "default config must open preview in LivePane mode",
        );
    }

    /// Setting `[watch.preview] default_content = "prompt_response"` in
    /// config restores the pre-feature shape: `p` opens straight into
    /// the text view. Both branches of the config knob must be wired
    /// through, not just the default.
    #[test]
    fn prompt_response_default_opens_preview_in_text_mode() {
        let mut app = App::with_config(cfg_with_prompt_default());
        seed_three_agents(&mut app);
        app.table_state.select(Some(0));
        press(&mut app, 'p');
        assert_eq!(
            app.preview.as_ref().unwrap().content,
            PreviewContent::PromptResponse,
        );
    }

    /// `f` (geometry) and `c` (content) are independent — composing them
    /// must produce all four combinations without one clobbering the
    /// other. This is the user-visible promise of the two-axis design.
    #[test]
    fn f_and_c_compose_independently() {
        // Pin the starting axis state explicitly so this test reads as
        // "all four (mode, content) combinations are reachable", not
        // "the current default + 3 toggles lands somewhere expected."
        let mut app = App::with_config(cfg_with_prompt_default());
        seed_three_agents(&mut app);
        app.table_state.select(Some(0));
        press(&mut app, 'p');

        // (Popup, PromptResponse) → press f → (Fullscreen, PromptResponse)
        press(&mut app, 'f');
        let p = app.preview.as_ref().unwrap();
        assert_eq!(p.mode, PreviewMode::Fullscreen);
        assert_eq!(p.content, PreviewContent::PromptResponse);

        // → press c → (Fullscreen, LivePane)
        press(&mut app, 'c');
        let p = app.preview.as_ref().unwrap();
        assert_eq!(p.mode, PreviewMode::Fullscreen);
        assert_eq!(p.content, PreviewContent::LivePane);

        // → press f again → (Popup, LivePane) — content survives
        // geometry flip
        press(&mut app, 'f');
        let p = app.preview.as_ref().unwrap();
        assert_eq!(p.mode, PreviewMode::Popup);
        assert_eq!(p.content, PreviewContent::LivePane);
    }

    /// Closing the preview must drop the cached pane capture so a stale
    /// snapshot doesn't leak into the next preview session (which might
    /// be on a different pane that happened to reuse the same `pane_id`
    /// across a tmux pane close + recreate).
    #[test]
    fn close_preview_clears_pane_capture_cache() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(0));
        press(&mut app, 'p');
        // Stuff the cache by hand — the actual capture path needs a real
        // tmux server, but the close-clears-cache invariant is purely
        // about the App field's lifecycle.
        app.pane_capture = Some(CapturedPane {
            pane_id: "%1".into(),
            text: "old screen".into(),
            fetched_at: std::time::Instant::now(),
        });
        press(&mut app, 'q');
        assert!(app.preview.is_none());
        assert!(app.pane_capture.is_none(), "cache must drop with preview");
    }

    /// Toggling content from `LivePane` back to `PromptResponse` drops
    /// the cache too — re-entering capture mode should fetch a fresh
    /// view rather than flash the last cached frame for half a second.
    #[test]
    fn toggle_to_prompt_drops_cache() {
        // Start from PromptResponse so the toggle direction this test
        // exercises (LivePane → PromptResponse) is unambiguous.
        let mut app = App::with_config(cfg_with_prompt_default());
        seed_three_agents(&mut app);
        app.table_state.select(Some(0));
        press(&mut app, 'p');
        press(&mut app, 'c'); // → LivePane
        app.pane_capture = Some(CapturedPane {
            pane_id: "%1".into(),
            text: "old".into(),
            fetched_at: std::time::Instant::now(),
        });
        press(&mut app, 'c'); // → PromptResponse
        assert!(
            app.pane_capture.is_none(),
            "leaving capture mode must invalidate cache",
        );
    }

    /// Capture-mode renderer must not panic on a missing or pane-id-
    /// mismatched cache: it returns a placeholder instead. Covers the
    /// "(capturing pane…)" first-frame path and the "(pane gone or
    /// capture failed)" empty-text path.
    #[test]
    fn pane_capture_body_falls_back_when_cache_missing_or_empty() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        // No cache yet → "(capturing pane…)"
        let body = build_pane_capture_body(&app, "%1");
        let dump: String = body
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(dump.contains("capturing pane"));

        // Cache present but for a different pane → still placeholder.
        app.pane_capture = Some(CapturedPane {
            pane_id: "%999".into(),
            text: "irrelevant".into(),
            fetched_at: std::time::Instant::now(),
        });
        let body = build_pane_capture_body(&app, "%1");
        let dump: String = body
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(dump.contains("capturing pane"));

        // Cache present, correct pane, but empty text → pane-gone hint.
        app.pane_capture = Some(CapturedPane {
            pane_id: "%1".into(),
            text: String::new(),
            fetched_at: std::time::Instant::now(),
        });
        let body = build_pane_capture_body(&app, "%1");
        let dump: String = body
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            dump.contains("pane gone") || dump.contains("capture failed"),
            "expected empty-cache placeholder, got {dump:?}",
        );
    }

    /// Footer in preview mode advertises the `c` content toggle. The
    /// label should reflect the *target* state so users see what the
    /// next press would do, not where they already are.
    #[test]
    fn preview_footer_advertises_c_toggle() {
        let backend = TestBackend::new(160, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(dump.contains(" c "), "footer must surface the c key");
        assert!(
            dump.contains("live pane"),
            "footer in PromptResponse mode must hint at flipping to live pane",
        );

        // Flip to LivePane and re-render — label should now point
        // back to "prompt".
        app.preview.as_mut().unwrap().content = PreviewContent::LivePane;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            dump.contains(" prompt"),
            "footer in LivePane mode must hint at flipping back to prompt",
        );
    }

    // ---- quick actions (K / R / c / ?) ----------------------------------

    /// Recorder stub for `Effects` — captures every call so tests can
    /// assert on what the dispatcher tried to do without spawning a
    /// real subprocess. Each method's response is configurable so we
    /// can exercise both success and failure branches.
    #[derive(Default)]
    struct RecorderEffects {
        kill_calls: Vec<String>,
        ctrl_c_calls: Vec<String>,
        copy_calls: Vec<String>,
        send_prompt_calls: Vec<(String, String)>,
        kill_result: Option<std::result::Result<(), String>>,
        ctrl_c_result: Option<std::result::Result<(), String>>,
        copy_result: Option<std::result::Result<String, String>>,
        send_prompt_result: Option<std::result::Result<(), String>>,
    }

    impl Effects for RecorderEffects {
        fn kill_pane(&mut self, pane_id: &str) -> std::result::Result<(), String> {
            self.kill_calls.push(pane_id.to_string());
            self.kill_result.clone().unwrap_or(Ok(()))
        }
        fn send_ctrl_c(&mut self, pane_id: &str) -> std::result::Result<(), String> {
            self.ctrl_c_calls.push(pane_id.to_string());
            self.ctrl_c_result.clone().unwrap_or(Ok(()))
        }
        fn copy_to_clipboard(&mut self, text: &str) -> std::result::Result<String, String> {
            self.copy_calls.push(text.to_string());
            self.copy_result
                .clone()
                .unwrap_or_else(|| Ok("pbcopy".into()))
        }
        fn send_prompt(&mut self, pane_id: &str, text: &str) -> std::result::Result<(), String> {
            self.send_prompt_calls
                .push((pane_id.to_string(), text.to_string()));
            self.send_prompt_result.clone().unwrap_or(Ok(()))
        }
    }

    /// Build an app that has a paneless agent (Claude SDK sub-process
    /// whose ancestry walk failed) at row 0 and a normal pane-bearing
    /// agent at row 1. Lets `K` / `R` / `c` tests drive against both
    /// shapes without re-doing the fixture each time.
    fn app_with_paneless_and_pane() -> App {
        let cfg = WatchConfig {
            view: WatchView::Pane,
            // Disable hide_paneless so the row stays in the table
            // — the picker default would filter it out and the test
            // would have to look elsewhere.
            hide_paneless: false,
            sort: vec![WatchSortKey::PaneId],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let paneless = fake_agent(
            "no-pane",
            None,
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        );
        let panefull = fake_agent(
            "with-pane",
            Some("%42"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hello world"),
            None,
            None,
            None,
        );
        app.set_data(
            vec![paneless, panefull],
            vec![fake_pane("%42", "main", 2, 0, "claude")],
        );
        app
    }

    #[test]
    fn kill_action_disabled_for_paneless_row() {
        let mut app = app_with_paneless_and_pane();
        // Find the paneless row by walking the rows — sort may have
        // moved it relative to insertion order.
        let paneless_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.is_none()))
            .expect("test fixture must include a paneless row");
        app.table_state.select(Some(paneless_idx));

        let action = quick_kill_action(&app);
        assert!(
            matches!(action, Action::NotApplicable(msg) if msg.contains("no tmux pane")),
            "K on a paneless row must yield NotApplicable, got something else"
        );
    }

    #[test]
    fn kill_action_opens_confirm_for_pane_bearing_row() {
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .expect("test fixture must include a pane-bearing row");
        app.table_state.select(Some(pane_idx));

        let action = quick_kill_action(&app);
        match action {
            Action::AskConfirm(popup) => {
                assert_eq!(popup.on_confirm, QuickAction::KillPane("%42".into()));
                // Pane label resolved against the inventory — should
                // be the human-readable form, not the raw `%42`.
                assert!(
                    popup.message.contains("main:2.0"),
                    "popup message should resolve pane label, got: {}",
                    popup.message
                );
            }
            other => panic!("expected AskConfirm, got {other:?}"),
        }
    }

    #[test]
    fn copy_action_uses_last_prompt() {
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .expect("test fixture must include a pane-bearing row");
        app.table_state.select(Some(pane_idx));

        let action = quick_copy_action(&app);
        match action {
            Action::Quick(QuickAction::CopyPrompt(text)) => {
                assert_eq!(text, "hello world");
            }
            other => panic!("expected Quick(CopyPrompt), got {other:?}"),
        }
    }

    #[test]
    fn copy_action_disabled_when_no_prompt() {
        let mut app = app_with_paneless_and_pane();
        // Paneless agent has no last_prompt either — covers the
        // "row exists but has nothing to copy" case.
        let paneless_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.is_none()))
            .unwrap();
        app.table_state.select(Some(paneless_idx));

        let action = quick_copy_action(&app);
        assert!(
            matches!(action, Action::NotApplicable(msg) if msg.contains("no prompt")),
            "c with no prompt must yield NotApplicable, got something else"
        );
    }

    #[test]
    fn enter_attaches_to_the_selected_pane() {
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .expect("test fixture must include a pane-bearing row");
        app.table_state.select(Some(pane_idx));

        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(
            matches!(action, Action::AttachPane(ref p) if p == "%42"),
            "Enter must attach without an intermediate composer, got {action:?}"
        );
    }

    #[test]
    fn enter_on_paneless_row_hints_instead_of_opening_prompt() {
        let mut app = app_with_paneless_and_pane();
        let paneless_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.is_none()))
            .expect("test fixture must include a paneless row");
        app.table_state.select(Some(paneless_idx));

        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(
            matches!(action, Action::NotApplicable(msg) if msg.contains("no tmux pane")),
            "Enter on a paneless row must yield NotApplicable, got something else"
        );
    }

    #[test]
    fn pasted_text_with_newline_goes_to_buffer_not_submit() {
        // Bracketed paste delivers the whole payload — newlines and all —
        // as one `Event::Paste`. It must land in the composer buffer, not
        // submit at the embedded `\n` the way a stream of key events would.
        let mut app = collaboration_watch_app();
        open_watch_collaboration_composer(&mut app);

        let action = handle_event(Event::Paste("line one\nline two".into()), &mut app);
        assert!(matches!(action, Action::None));
        assert_eq!(
            app.collaboration_composer.as_ref().unwrap().input,
            "line one\nline two"
        );

        // A subsequent real Enter is what submits.
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::SubmitCollaboration));
    }

    #[test]
    fn paste_outside_composer_becomes_search_query() {
        let mut app = app_with_paneless_and_pane();
        let action = handle_event(Event::Paste("junk".into()), &mut app);
        assert!(matches!(action, Action::None));
        assert_eq!(app.search_query, "junk");
    }

    #[test]
    fn confirm_popup_y_proceeds() {
        // Drives the popup state machine end-to-end: open the popup
        // via `K`, press `y`, observe that ConfirmYes lands and that
        // dispatching the payload calls the right Effects method.
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .unwrap();
        app.table_state.select(Some(pane_idx));

        // Open the confirm popup the same way `run` would.
        let Action::AskConfirm(popup) = quick_kill_action(&app) else {
            panic!("expected AskConfirm");
        };
        app.confirm = Some(popup);

        // Press `y` — handle_event must yield ConfirmYes.
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::ConfirmYes));

        // Mirror the run-loop dispatch: take the popup's payload and
        // route it through dispatch_quick_action.
        let payload = app.confirm.take().unwrap().on_confirm;
        let mut fx = RecorderEffects::default();
        let outcome = dispatch_quick_action(payload, &mut fx);
        assert_eq!(fx.kill_calls, vec!["%42"]);
        assert!(matches!(outcome, ActionOutcome::Ok(msg) if msg.contains("killed pane %42")));
    }

    #[test]
    fn confirm_popup_n_cancels() {
        let mut app = app_with_paneless_and_pane();
        app.confirm = Some(ConfirmPopup {
            message: "Kill pane main:2.0?".into(),
            on_confirm: QuickAction::KillPane("%42".into()),
        });

        // `n` is one of "anything that isn't y/Y/Enter" — must cancel.
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::ConfirmCancel));
    }

    #[test]
    fn confirm_popup_esc_tab_arrow_all_cancel() {
        // Regression guard for the safety rail: keys you'd plausibly
        // hit by accident (Tab when switching focus, arrow when trying
        // to scroll, Esc when changing your mind) must NOT confirm.
        for key in [
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('q'),
            KeyCode::Char('K'), // even the same key that opened it
        ] {
            let mut app = app_with_paneless_and_pane();
            app.confirm = Some(ConfirmPopup {
                message: "Kill pane?".into(),
                on_confirm: QuickAction::KillPane("%42".into()),
            });
            let action = handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app);
            assert!(
                matches!(action, Action::ConfirmCancel),
                "key {key:?} must cancel the confirm popup"
            );
        }
    }

    #[test]
    fn confirm_popup_enter_proceeds() {
        // Enter is also an accept gate — matches the convention from
        // most other terminal y/N prompts. Spec calls this out
        // explicitly.
        let mut app = app_with_paneless_and_pane();
        app.confirm = Some(ConfirmPopup {
            message: "Kill pane?".into(),
            on_confirm: QuickAction::KillPane("%42".into()),
        });
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::ConfirmYes));
    }

    #[test]
    fn help_overlay_lists_every_binding() {
        // Snapshot test: the help overlay is the user's canonical
        // reference, so any drift between the keybinding matrix and
        // the help text should land here loud and clear.
        let body = help_overlay_text().join("\n");
        assert!(body.contains("type or /       filter"));
        assert!(body.contains("gg/G · Home/End first / last selectable row"));
        assert!(body.contains("↑/↓ · j/k       move sessions/children"));
        assert!(body.contains(":              command palette"));
        assert!(body.contains("Alt-A          attention-only filter"));
        assert!(body.contains("Alt-S/L/D/T    session / latest / duration / state"));
        assert!(body.contains("Alt-I / Alt-E  inspector / persistent event inbox"));
        assert!(body.contains("m / b          message selected room peer / mailbox"));
        assert!(body.contains("i / e          (in mailbox) claim inbox / reply"));
        assert!(body.contains("toggle help; q/Ctrl-C quits"));
    }

    #[test]
    fn f1_toggles_help_overlay() {
        let mut app = app_with_paneless_and_pane();
        assert!(!app.help_open);

        // First press — opens.
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::Quick(QuickAction::ShowHelp)));
        // Mirror the run-loop dispatch.
        if let Action::Quick(QuickAction::ShowHelp) = action {
            app.help_open = !app.help_open;
        }
        assert!(app.help_open, "first ? press must open the overlay");

        // Second press — closes. With help open, only F1 / Alt-? / Esc
        // are accepted; everything else is ignored to avoid
        // double-binding.
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::Quick(QuickAction::ShowHelp)));
        if let Action::Quick(QuickAction::ShowHelp) = action {
            app.help_open = !app.help_open;
        }
        assert!(!app.help_open, "second ? press must close the overlay");
    }

    #[test]
    fn help_overlay_swallows_other_keys() {
        // While help is open, K / R / c shouldn't fire — the user is
        // reading docs, not driving actions.
        let mut app = app_with_paneless_and_pane();
        app.help_open = true;

        for key in [
            KeyCode::Char('K'),
            KeyCode::Char('R'),
            KeyCode::Char('c'),
            KeyCode::Char('p'),
            KeyCode::Char('r'),
        ] {
            let action = handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app);
            assert!(
                matches!(action, Action::None),
                "while help is open, key {key:?} must be a no-op"
            );
        }
    }

    #[test]
    fn dispatch_kill_pane_calls_effects_and_reports_pane_id() {
        let mut fx = RecorderEffects::default();
        let outcome = dispatch_quick_action(QuickAction::KillPane("%99".into()), &mut fx);
        assert_eq!(fx.kill_calls, vec!["%99"]);
        match outcome {
            ActionOutcome::Ok(msg) => assert!(msg.contains("%99")),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_kill_pane_reports_failure_with_message() {
        let mut fx = RecorderEffects {
            kill_result: Some(Err("tmux exited with 1".into())),
            ..Default::default()
        };
        let outcome = dispatch_quick_action(QuickAction::KillPane("%99".into()), &mut fx);
        match outcome {
            ActionOutcome::Err(msg) => {
                assert!(msg.contains("kill-pane failed"));
                assert!(msg.contains("tmux exited with 1"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_copy_prompt_pipes_through_effects() {
        let mut fx = RecorderEffects::default();
        let outcome = dispatch_quick_action(QuickAction::CopyPrompt("hi there".into()), &mut fx);
        assert_eq!(fx.copy_calls, vec!["hi there"]);
        assert!(matches!(outcome, ActionOutcome::Ok(msg) if msg.contains("copied prompt")));
    }

    #[test]
    fn dispatch_copy_prompt_via_tmpfile_path_surfaces_warning_text() {
        // When all clipboard helpers are missing the dispatcher writes
        // to /tmp and reports the path so the user can recover their
        // text rather than being told "copied" with nothing in their
        // clipboard.
        let mut fx = RecorderEffects {
            copy_result: Some(Ok("tmpfile:/tmp/muxa-clip-1.txt".into())),
            ..Default::default()
        };
        let outcome = dispatch_quick_action(QuickAction::CopyPrompt("payload".into()), &mut fx);
        match outcome {
            ActionOutcome::Ok(msg) => {
                assert!(msg.contains("/tmp/muxa-clip-1.txt"));
                assert!(msg.contains("no clipboard tool"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_send_prompt_calls_effects_and_reports_pane_id() {
        let mut fx = RecorderEffects::default();
        let outcome = dispatch_quick_action(
            QuickAction::SendPrompt {
                pane_id: "%99".into(),
                text: "continue".into(),
            },
            &mut fx,
        );
        assert_eq!(
            fx.send_prompt_calls,
            vec![("%99".into(), "continue".into())]
        );
        assert!(matches!(outcome, ActionOutcome::Ok(msg) if msg.contains("%99")));
    }

    #[test]
    fn dispatch_send_prompt_reports_failure_with_message() {
        let mut fx = RecorderEffects {
            send_prompt_result: Some(Err("tmux exited with 1".into())),
            ..Default::default()
        };
        let outcome = dispatch_quick_action(
            QuickAction::SendPrompt {
                pane_id: "%99".into(),
                text: "continue".into(),
            },
            &mut fx,
        );
        match outcome {
            ActionOutcome::Err(msg) => {
                assert!(msg.contains("send failed"));
                assert!(msg.contains("tmux exited with 1"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn tmux_prompt_send_waits_before_submit_key() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut sleeps = Vec::new();

        let result = send_prompt_to_tmux(
            "%42",
            "hello",
            Duration::from_millis(120),
            |args| {
                calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
                Ok(())
            },
            |delay| sleeps.push(delay),
        );

        assert!(result.is_ok());
        assert_eq!(
            calls,
            vec![
                vec!["send-keys", "-t", "%42", "-l", "--", "hello"],
                vec!["send-keys", "-t", "%42", "Enter"],
            ]
        );
        assert_eq!(sleeps, vec![Duration::from_millis(120)]);
    }

    #[test]
    fn tmux_prompt_send_skips_submit_when_literal_input_fails() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut sleeps = Vec::new();

        let result = send_prompt_to_tmux(
            "%42",
            "hello",
            Duration::from_millis(120),
            |args| {
                calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
                Err("tmux exited with 1".into())
            },
            |delay| sleeps.push(delay),
        );

        assert_eq!(result.unwrap_err(), "tmux exited with 1");
        assert_eq!(
            calls,
            vec![vec!["send-keys", "-t", "%42", "-l", "--", "hello"]]
        );
        assert!(sleeps.is_empty());
    }

    #[test]
    fn j_and_k_navigate_until_another_letter_starts_filtering() {
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
            .unwrap();
        app.table_state.select(Some(pane_idx));

        let action = key_action(&mut app, 'k');
        assert!(matches!(action, Action::None));
        assert!(app.search_query.is_empty());

        let action = key_action(&mut app, 'e');
        assert!(matches!(action, Action::None));
        let action = key_action(&mut app, 'r');
        assert!(matches!(action, Action::None));
        let action = key_action(&mut app, 'k');
        assert!(matches!(action, Action::None));
        assert_eq!(app.search_query, "erk");

        app.edit_search(String::clear);
        assert!(matches!(alt_key_action(&mut app, 'r'), Action::Refresh));
        assert!(matches!(
            alt_key_action(&mut app, 'k'),
            Action::AskConfirm(_)
        ));
    }

    #[test]
    fn alt_i_says_which_way_the_inspector_went() {
        // The inspector starts enabled, so the first Alt-I hides a panel
        // instead of summoning one — and under 120 columns neither state
        // renders anything. Silence made a working binding get reported
        // as "only works on the second press".
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        assert!(app.inspector_enabled, "inspector is on by default");

        assert!(matches!(alt_key_action(&mut app, 'i'), Action::None));
        assert!(!app.inspector_enabled);
        assert_eq!(
            app.footer_hint.as_ref().map(|h| h.message.as_str()),
            Some("inspector disabled")
        );

        assert!(matches!(alt_key_action(&mut app, 'i'), Action::None));
        assert!(app.inspector_enabled);
        assert_eq!(
            app.footer_hint.as_ref().map(|h| h.message.as_str()),
            Some("inspector enabled")
        );
    }

    #[test]
    fn clearing_filter_restores_j_and_k_navigation() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(0));

        assert!(matches!(key_action(&mut app, 'j'), Action::None));
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));
        assert!(app.search_query.is_empty());

        assert!(matches!(key_action(&mut app, 'e'), Action::None));
        assert!(matches!(key_action(&mut app, 'j'), Action::None));
        assert!(matches!(key_action(&mut app, 'k'), Action::None));
        assert_eq!(app.search_query, "ejk");

        app.edit_search(String::clear);
        assert!(matches!(key_action(&mut app, 'k'), Action::None));
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn direct_typing_filters_and_escape_clears_before_quit() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(0));

        for c in "eta".chars() {
            assert!(matches!(key_action(&mut app, c), Action::None));
        }
        assert_eq!(app.search_query, "eta");
        assert_eq!(app.visible_targets().len(), 1);
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));

        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::None));
        assert!(app.search_query.is_empty());
        assert_eq!(app.visible_targets().len(), 3);

        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::Quit));
    }

    #[test]
    fn slash_search_accepts_reserved_keys_and_restores_browse_shortcuts() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());

        assert!(matches!(key_action(&mut app, '/'), Action::None));
        assert!(app.explicit_search);
        for c in "qghlro?".chars() {
            assert!(matches!(key_action(&mut app, c), Action::None));
        }
        assert_eq!(app.search_query, "qghlro?");

        let clear = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            &mut app,
        );
        assert!(matches!(clear, Action::None));
        assert!(app.search_query.is_empty());
        assert!(!app.explicit_search);
        assert!(matches!(key_action(&mut app, 'q'), Action::Quit));
    }

    #[test]
    fn explicit_search_stays_armed_after_backspacing_to_empty() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        assert!(matches!(key_action(&mut app, '/'), Action::None));
        assert!(matches!(key_action(&mut app, 'q'), Action::None));
        assert!(matches!(
            handle_event(
                Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
                &mut app,
            ),
            Action::None
        ));
        assert!(app.search_query.is_empty());
        assert!(app.explicit_search);

        assert!(matches!(key_action(&mut app, 'q'), Action::None));
        assert_eq!(app.search_query, "q");
    }

    #[test]
    fn conventional_browse_keys_cover_boundaries_and_pages() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_page_rows = 2;
        app.table_state.select(Some(0));

        for (key, expected) in [
            (KeyCode::End, 2),
            (KeyCode::Home, 0),
            (KeyCode::PageDown, 2),
            (KeyCode::PageUp, 0),
        ] {
            assert!(matches!(
                handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app,),
                Action::None
            ));
            assert_eq!(app.table_state.selected(), Some(expected));
        }

        assert!(matches!(
            handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL,)),
                &mut app,
            ),
            Action::None
        ));
        assert_eq!(app.table_state.selected(), Some(1));
        assert!(matches!(
            handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL,)),
                &mut app,
            ),
            Action::None
        ));
        assert_eq!(app.table_state.selected(), Some(0));

        assert!(matches!(key_action(&mut app, 'G'), Action::None));
        assert_eq!(app.table_state.selected(), Some(2));
        assert!(matches!(key_action(&mut app, 'g'), Action::None));
        assert!(app.pending_g);
        assert!(matches!(key_action(&mut app, 'g'), Action::None));
        assert_eq!(app.table_state.selected(), Some(0));
        assert!(!app.pending_g);
    }

    #[test]
    fn empty_filter_exposes_common_single_key_actions() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        assert!(matches!(key_action(&mut app, 'r'), Action::Refresh));
        assert!(matches!(key_action(&mut app, 'o'), Action::OpenPreview));
        assert!(matches!(
            key_action(&mut app, '?'),
            Action::Quick(QuickAction::ShowHelp)
        ));

        assert!(matches!(key_action(&mut app, 'e'), Action::None));
        for c in ['r', 'o', 'q', '?'] {
            assert!(matches!(key_action(&mut app, c), Action::None));
        }
        assert_eq!(app.search_query, "eroq?");
    }

    #[test]
    fn command_palette_completes_and_dispatches_safe_actions() {
        let mut app = app_with_paneless_and_pane();
        let pane_idx = app
            .rows
            .iter()
            .position(
                |row| matches!(row, WatchRow::Agent(agent) if agent.pane.as_deref() == Some("%42")),
            )
            .unwrap();
        app.table_state.select(Some(pane_idx));

        assert!(matches!(key_action(&mut app, ':'), Action::None));
        for c in "sort l".chars() {
            assert!(matches!(key_action(&mut app, c), Action::None));
        }
        assert!(matches!(
            handle_event(
                Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                &mut app,
            ),
            Action::None
        ));
        assert_eq!(
            app.command_palette.as_ref().map(|c| c.input.as_str()),
            Some("sort latest")
        );
        assert!(matches!(
            handle_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                &mut app,
            ),
            Action::SetSort(WatchSortPreset::Latest)
        ));
        assert!(app.command_palette.is_none());

        assert!(matches!(key_action(&mut app, ':'), Action::None));
        for c in "kill".chars() {
            assert!(matches!(key_action(&mut app, c), Action::None));
        }
        assert!(matches!(
            handle_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                &mut app,
            ),
            Action::AskConfirm(_)
        ));
    }

    #[test]
    fn command_view_rebuilds_cached_rows_and_preserves_a_pane_target() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(1));
        app.paneless_hidden = 2;
        app.paneless_attention = 1;
        let selected = app.selected_pane();

        app.apply_view(WatchView::Session);
        assert!(app
            .rows
            .iter()
            .all(|row| matches!(row, WatchRow::Session(_))));
        assert!(app.columns.contains(&WatchColumn::SessionTime));
        assert_eq!(app.selected_pane(), selected);
        assert_eq!(app.paneless_hidden, 2);
        assert_eq!(app.paneless_attention, 1);

        app.apply_view(WatchView::Pane);
        assert!(app.rows.iter().any(|row| matches!(row, WatchRow::Agent(_))));
        assert_eq!(app.selected_pane(), selected);
    }

    #[test]
    fn command_palette_renders_input_and_matching_suggestions() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.command_palette = Some(CommandPalette {
            input: "sort".into(),
            cursor: 4,
        });

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen = (0..terminal.backend().buffer().area().height)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("commands · Enter run"));
        assert!(screen.contains(": sort"));
        assert!(screen.contains("sort latest"));
        assert!(screen.contains("sort duration"));
    }

    #[test]
    fn attention_filter_keeps_only_blocked_agents() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        let WatchRow::Agent(agent) = &mut app.rows[1] else {
            panic!("expected agent row");
        };
        agent.state = AgentState::WaitingInput;

        assert!(matches!(alt_key_action(&mut app, 'a'), Action::None));
        assert!(app.attention_only);
        assert_eq!(app.visible_targets().len(), 1);
        assert_eq!(app.selected_pane().as_deref(), Some("%2"));
    }

    #[test]
    fn selected_session_auto_expands_and_children_are_exact_action_targets() {
        let mut app = session_preview_app();
        assert_eq!(app.visible_targets().len(), 3);
        assert!(matches!(
            app.selected_identity(),
            Some(RowIdentity::Session(_))
        ));
        let expected_child_pane = match &app.rows[0] {
            WatchRow::Session(session) => session.agents[0].pane.clone(),
            _ => None,
        };
        let expected_second_child_pane = match &app.rows[0] {
            WatchRow::Session(session) => session.agents[1].pane.clone(),
            _ => None,
        };

        let _ = key_action(&mut app, 'l');
        assert_eq!(app.visible_targets().len(), 3);
        assert_eq!(app.selected_pane(), expected_child_pane);
        app.move_down();
        assert_eq!(app.selected_pane(), expected_second_child_pane);
        app.move_down();
        assert_eq!(app.selected_pane(), expected_child_pane);
        let Action::AskConfirm(popup) = quick_kill_action(&app) else {
            panic!("expanded child should be killable");
        };
        assert_eq!(
            popup.on_confirm,
            QuickAction::KillPane(app.selected_pane().unwrap())
        );

        let _ = key_action(&mut app, 'h');
        assert_eq!(app.visible_targets().len(), 3);
        assert!(matches!(
            app.selected_identity(),
            Some(RowIdentity::Session(_))
        ));
    }

    #[test]
    fn moving_to_another_session_folds_the_previous_and_opens_the_new_one() {
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let now = OffsetDateTime::now_utc();
        app.set_data_with_sessions(
            vec![
                fake_agent_at("alpha", "%1", now),
                fake_agent_at("beta", "%2", now),
            ],
            vec![
                fake_pane("%1", "alpha", 0, 0, "claude"),
                fake_pane("%3", "alpha", 0, 1, "zsh"),
                fake_pane("%2", "beta", 0, 0, "codex"),
                fake_pane("%4", "beta", 0, 1, "zsh"),
            ],
            vec![
                fake_session("$1", "alpha", 1),
                fake_session("$2", "beta", 1),
            ],
            vec![],
        );

        let initial = app.visible_targets();
        assert_eq!(initial.len(), 3);
        assert_eq!(initial[0].row_idx, 0);
        assert_eq!(initial[1].agent_idx, Some(0));
        assert_eq!(initial[2].row_idx, 1);

        app.move_down();

        let selected = app.selected_target().expect("second session selected");
        assert_eq!(selected.row_idx, 1);
        assert_eq!(selected.agent_idx, None);
        let switched = app.visible_targets();
        assert_eq!(switched.len(), 3);
        assert_eq!(switched[0].row_idx, 0);
        assert_eq!(switched[0].agent_idx, None);
        assert_eq!(switched[1].row_idx, 1);
        assert_eq!(switched[1].agent_idx, None);
        assert_eq!(switched[2].row_idx, 1);
        assert_eq!(switched[2].agent_idx, Some(0));
    }

    #[test]
    fn single_pane_session_keeps_detail_without_a_redundant_child_row() {
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let now = OffsetDateTime::now_utc();
        let mut agent = fake_agent_at("solo-agent", "%1", now);
        agent.last_response = Some("solo detail remains visible".into());
        app.set_data_with_sessions(
            vec![agent],
            vec![fake_pane("%1", "solo", 0, 0, "codex")],
            vec![fake_session("$1", "solo", 1)],
            vec![],
        );

        assert_eq!(app.visible_targets().len(), 1);
        app.move_into_session();
        assert_eq!(app.visible_targets().len(), 1);
        assert!(matches!(
            app.selected_identity(),
            Some(RowIdentity::Session(_))
        ));

        let backend = TestBackend::new(110, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            dump.contains("↳ solo detail remains visible"),
            "single-pane detail missing: {dump:?}"
        );
        let session_line = row_text(terminal.backend().buffer(), 5);
        assert!(
            !session_line.contains('▸') && !session_line.contains('▾'),
            "single pane must not show an expansion marker: {session_line:?}"
        );
    }

    #[test]
    fn single_and_multi_pane_session_names_start_in_the_same_column() {
        let cfg = WatchConfig {
            view: WatchView::Session,
            sort: vec![WatchSortKey::Session],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        let now = OffsetDateTime::now_utc();
        app.set_data_with_sessions(
            vec![
                fake_agent_at("one-agent", "%1", now),
                fake_agent_at("two-agent", "%2", now),
            ],
            vec![
                fake_pane("%1", "one-pane", 0, 0, "codex"),
                fake_pane("%2", "two-pane", 0, 0, "claude"),
                fake_pane("%3", "two-pane", 0, 1, "zsh"),
            ],
            vec![
                fake_session("$1", "one-pane", 1),
                fake_session("$2", "two-pane", 1),
            ],
            vec![],
        );

        let backend = TestBackend::new(110, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let lines: Vec<String> = (0..terminal.backend().buffer().area.height)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect();
        let one = lines
            .iter()
            .find(|line| line.contains("one-pane"))
            .expect("single-pane row");
        let two = lines
            .iter()
            .find(|line| line.contains("two-pane"))
            .expect("multi-pane row");
        assert_eq!(
            one.find("one-pane"),
            two.find("two-pane"),
            "session labels must share one left edge: {one:?} / {two:?}"
        );
    }

    #[test]
    fn selected_session_and_selected_child_both_render_detail() {
        let mut app = session_preview_app();
        let expected_detail = match &app.rows[0] {
            WatchRow::Session(session) => session.agents[0]
                .last_response
                .clone()
                .expect("first child response"),
            _ => panic!("expected session row"),
        };
        let backend = TestBackend::new(140, 16);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| render(f, &mut app)).unwrap();
        let parent_detail = row_text(terminal.backend().buffer(), 6);
        assert!(
            parent_detail.contains('↳') && parent_detail.contains(&expected_detail),
            "parent detail missing above children: {parent_detail:?}"
        );

        app.move_into_session();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let child_detail = row_text(terminal.backend().buffer(), 7);
        assert!(
            child_detail.contains('↳') && child_detail.contains(&expected_detail),
            "selected child detail missing: {child_detail:?}"
        );
    }

    #[test]
    fn state_age_cell_shows_state_and_time_in_state() {
        let now = time::macros::datetime!(2026-08-03 12:00:00 UTC);
        let mut agent = fake_agent_at("waiting", "%1", now);
        agent.state = AgentState::WaitingInput;
        agent.state_entered_at = now - time::Duration::minutes(5);
        let text = state_age_text(&agent, now, watch_theme(WatchTheme::Classic), Spinner::OFF);
        assert_eq!(plain_text(&text), "▶ WAIT 5m");
    }

    #[test]
    fn transitions_stay_in_event_inbox_until_opened() {
        let mut app = App::new();
        let working = fake_agent(
            "event-agent",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("finish the task"),
            None,
            None,
            None,
        );
        app.set_data(
            vec![working.clone()],
            vec![fake_pane("%1", "main", 0, 0, "claude")],
        );
        app.detect_pulses();

        let mut done = working;
        done.state = AgentState::Idle;
        done.state_entered_at = OffsetDateTime::now_utc();
        apply_outcome(&mut app, RefreshOutcome::SingleAgent(done));

        assert_eq!(app.events.len(), 1);
        assert_eq!(app.events[0].kind, WatchEventKind::Done);
        assert_eq!(app.unread_events, 1);
        app.toggle_event_inbox();
        assert!(app.event_inbox_open);
        assert_eq!(app.unread_events, 0);
    }

    #[test]
    fn footer_hint_set_by_apply_outcome() {
        let mut app = app_with_paneless_and_pane();
        apply_outcome_to_app(&mut app, ActionOutcome::Ok("✔ ok".into()));
        let hint = app.footer_hint.as_ref().expect("hint must be set");
        assert_eq!(hint.message, "✔ ok");
        assert_eq!(hint.level, HintLevel::Ok);
        assert!(hint.fresh());

        apply_outcome_to_app(&mut app, ActionOutcome::Err("✗ bad".into()));
        assert_eq!(app.footer_hint.as_ref().unwrap().level, HintLevel::Err);
    }

    #[test]
    fn confirm_popup_renders_without_panic_and_includes_message() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_paneless_and_pane();
        app.confirm = Some(ConfirmPopup {
            message: "Kill pane main:2.0?".into(),
            on_confirm: QuickAction::KillPane("%42".into()),
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            dump.contains("Kill pane main:2.0?"),
            "missing message: {dump:?}"
        );
        assert!(dump.contains("[y]"), "missing [y]: {dump:?}");
        assert!(dump.contains("[N]"), "missing [N]: {dump:?}");
    }

    #[test]
    fn help_popup_renders_without_panic_and_includes_keybindings() {
        // Terminal sized generously so the 60 × 90 % help popup has
        // room for the full keybinding matrix; smaller terminals
        // would clip the bottom sections and the test would fail
        // for cosmetic rather than logical reasons.
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_paneless_and_pane();
        app.help_open = true;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            dump.contains("Filter & navigation"),
            "missing filter help: {dump:?}"
        );
        assert!(
            dump.contains("Quick actions"),
            "missing Quick actions: {dump:?}",
        );
        assert!(dump.contains("kill the pane"), "missing kill: {dump:?}");
    }

    // ---- visual snapshots --------------------------------------------------
    //
    // Pin the full rendered buffer as a plain-text snapshot so render
    // regressions get caught beyond "did not panic". Helpers strip styling
    // (colours don't affect logical layout, and styling deltas would churn
    // snapshots) and normalize the relative-time column ("12s" →
    // "<rel>") because the production `render` path calls
    // `OffsetDateTime::now_utc()` directly and we'd rather not thread an
    // injectable clock through it just for tests.

    mod snapshot_helpers {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        /// Flatten the `TestBackend` buffer into a single visual string.
        /// One line per buffer row, trailing whitespace trimmed per line,
        /// relative-time tokens normalized so snapshots stay stable.
        pub(super) fn buffer_string(terminal: &Terminal<TestBackend>) -> String {
            let buf = terminal.backend().buffer();
            let area = buf.area();
            let mut out =
                String::with_capacity(usize::from(area.width + 1) * usize::from(area.height));
            for y in 0..area.height {
                let mut row = String::with_capacity(usize::from(area.width));
                for x in 0..area.width {
                    row.push_str(buf.cell((x, y)).map_or("", ratatui::buffer::Cell::symbol));
                }
                let row = row.trim_end();
                out.push_str(&normalize_relative_time(row));
                out.push('\n');
            }
            out
        }

        /// Replace compact `\d+(s|m|h|d)` tokens with `<rel>` and `HH:MM:SS UTC` with
        /// `<clock>` so neither the activity column nor the header clock
        /// drifts across runs. (`render_header` reads `app.last_refresh`,
        /// which is set to `now_utc()` inside `App::with_config`/`set_data`
        /// — not injectable without a production-code change.)
        fn normalize_relative_time(s: &str) -> String {
            let bytes = s.as_bytes();
            let mut out = String::with_capacity(s.len());
            let mut i = 0;
            while i < bytes.len() {
                if let Some(consumed) = match_clock(&bytes[i..]) {
                    out.push_str("<clock>");
                    i += consumed;
                    continue;
                }
                let digit_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i > digit_start
                    && i < bytes.len()
                    && matches!(bytes[i], b's' | b'm' | b'h' | b'd')
                    && bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace)
                {
                    // Eat the digits + unit AND any spaces that follow
                    // (column padding). The relative-time text is
                    // variable-length (2-4 chars) so without this the
                    // column-trailing whitespace would drift across runs
                    // as the value crosses the 60s / 60m / 48h boundaries.
                    // Keep one separator so adjacent columns still read like
                    // the production table after normalization.
                    i += 1;
                    let space_start = i;
                    while i < bytes.len() && bytes[i] == b' ' {
                        i += 1;
                    }
                    out.push_str("<rel>");
                    if i > space_start {
                        out.push(' ');
                    }
                } else {
                    // Not a relative-time token — copy the digit run and the
                    // following char (if any) verbatim.
                    out.push_str(&s[digit_start..i]);
                    if i < bytes.len() {
                        let ch_start = i;
                        // Advance one UTF-8 char.
                        i += 1;
                        while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                            i += 1;
                        }
                        out.push_str(&s[ch_start..i]);
                    }
                }
            }
            out
        }

        /// Return `Some(12)` if `b` starts with `HH:MM:SS UTC`, else `None`.
        fn match_clock(b: &[u8]) -> Option<usize> {
            if b.len() < 12 {
                return None;
            }
            let d = |i: usize| b[i].is_ascii_digit();
            if d(0)
                && d(1)
                && b[2] == b':'
                && d(3)
                && d(4)
                && b[5] == b':'
                && d(6)
                && d(7)
                && &b[8..12] == b" UTC"
            {
                Some(12)
            } else {
                None
            }
        }
    }

    fn snapshot_app() -> App {
        // Pin sort to PaneId so row order doesn't drift on activity jitter
        // and disable hide_paneless so the paneless row stays visible in
        // the mixed-list snapshot. Pin pane view too so these row-rendering
        // snapshots stay focused on per-row visuals (state glyphs, columns,
        // selection) independent of the production default (`session`).
        let cfg = WatchConfig {
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            hide_paneless: false,
            // Static glyphs so the golden snapshots pin layout, not an
            // animation frame. The spinner has its own dedicated test.
            spinner: false,
            ..WatchConfig::default()
        };
        App::with_config(cfg)
    }

    #[test]
    fn snapshot_empty_state() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn oh_my_muxa_theme_renders_polished_chrome() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::with_config(WatchConfig {
            theme: Some(WatchTheme::OhMyMuxa),
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            ..WatchConfig::default()
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump = snapshot_helpers::buffer_string(&terminal);

        assert!(dump.contains("oh-my-muxa"));
        assert!(
            dump.contains("╭") && dump.contains("╯"),
            "oh-my-muxa should switch watch chrome to rounded borders"
        );
    }

    #[test]
    fn watch_theme_presets_render_named_chrome() {
        for (theme, title) in [
            (WatchTheme::Focus, "muxa focus"),
            (WatchTheme::Ops, "muxa ops"),
            (WatchTheme::Mono, "muxa mono"),
            (WatchTheme::HighContrast, "muxa high-contrast"),
            (WatchTheme::Minimal, "muxa"),
        ] {
            let backend = TestBackend::new(100, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = App::with_config(WatchConfig {
                theme: Some(theme),
                view: WatchView::Pane,
                sort: vec![WatchSortKey::PaneId],
                ..WatchConfig::default()
            });
            terminal.draw(|f| render(f, &mut app)).unwrap();
            let dump = snapshot_helpers::buffer_string(&terminal);

            assert!(
                dump.contains(title),
                "expected theme title {title:?} in render dump:\n{dump}"
            );
        }
    }

    #[test]
    fn snapshot_row_working() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        app.set_data(
            vec![fake_agent(
                "s-work",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("refactor the ipc module"),
                Some("Opus"),
                Some(34.0),
                Some(0.12),
            )],
            vec![fake_pane("%1", "alpha", 0, 0, "claude")],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_row_waiting_input() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        app.set_data(
            vec![fake_agent(
                "s-wi",
                Some("%2"),
                AgentKind::Codex,
                AgentState::WaitingInput,
                Some("approve permission?"),
                None,
                None,
                None,
            )],
            vec![fake_pane("%2", "alpha", 1, 0, "codex")],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_row_waiting_choice() {
        // Regression guard: WaitingChoice is the newest AgentState variant
        // and the one we most want to pin visually so future styling/label
        // changes show up in review.
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        app.set_data(
            vec![fake_agent(
                "s-wc",
                Some("%3"),
                AgentKind::ClaudeCode,
                AgentState::WaitingChoice,
                Some("pick a plan"),
                None,
                None,
                None,
            )],
            vec![fake_pane("%3", "alpha", 2, 0, "claude")],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_row_error() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        app.set_data(
            vec![fake_agent(
                "s-err",
                Some("%4"),
                AgentKind::GeminiCli,
                AgentState::Error,
                Some("boom"),
                None,
                None,
                None,
            )],
            vec![fake_pane("%4", "alpha", 3, 0, "gemini")],
        );
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_mixed_list_with_selection() {
        // One agent per state, plus a paneless agent — selection on the
        // second row so the detail-line expansion is exercised too.
        let backend = TestBackend::new(110, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        let mut a1 = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("alpha prompt"),
            Some("Opus"),
            Some(7.0),
            Some(0.05),
        );
        a1.last_response = Some("alpha response".into());
        let mut a2 = fake_agent(
            "s2",
            Some("%2"),
            AgentKind::Codex,
            AgentState::WaitingInput,
            Some("beta prompt"),
            None,
            None,
            None,
        );
        a2.last_response = Some("beta response".into());
        let mut a3 = fake_agent(
            "s3",
            Some("%3"),
            AgentKind::ClaudeCode,
            AgentState::WaitingChoice,
            Some("gamma prompt"),
            None,
            None,
            None,
        );
        a3.last_response = Some("gamma response".into());
        let a4 = fake_agent(
            "s4",
            None,
            AgentKind::Unknown,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        );
        app.set_data(
            vec![a1, a2, a3, a4],
            vec![
                fake_pane("%1", "alpha", 0, 0, "claude"),
                fake_pane("%2", "alpha", 1, 0, "codex"),
                fake_pane("%3", "alpha", 2, 0, "claude"),
            ],
        );
        app.table_state.select(Some(1));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_help_overlay() {
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                Some("hi"),
                None,
                None,
                None,
            )],
            vec![fake_pane("%1", "alpha", 0, 0, "claude")],
        );
        app.help_open = true;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_confirm_popup() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        app.set_data(
            vec![fake_agent(
                "s1",
                Some("%42"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("hello"),
                None,
                None,
                None,
            )],
            vec![fake_pane("%42", "main", 2, 0, "claude")],
        );
        app.confirm = Some(ConfirmPopup {
            message: "Kill pane main:2.0?".into(),
            on_confirm: QuickAction::KillPane("%42".into()),
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }

    #[test]
    fn snapshot_preview_popup() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = snapshot_app();
        let mut a = fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("the prompt body"),
            Some("Opus"),
            Some(12.0),
            Some(0.03),
        );
        a.last_response = Some("the response body".into());
        app.set_data(vec![a], vec![fake_pane("%1", "alpha", 0, 0, "claude")]);
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();
        insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
    }
}
