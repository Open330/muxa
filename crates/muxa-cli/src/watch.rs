//! `muxa watch` — fullscreen ratatui dashboard.
//!
//! Polls the daemon via `Client::snapshot()` every 500 ms and renders a
//! live-updating table of tracked agents. Input is handled via crossterm
//! events (`q`/`Esc`/`Ctrl-C` to quit, `r` to force-refresh, `↑/↓` or
//! `j/k` for selection, `Enter` to attach into the selected pane,
//! `p` to pop open a full-screen preview of the selected row's prompt
//! and response — `q`/`Esc`/`p` returns to the table).
//!
//! Terminal lifecycle is managed by a RAII `TerminalGuard` so raw mode and
//! the alternate screen are always restored — even on panic.

use std::future::Future;
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
use muxa::config::{WatchConfig, WatchSortKey, WidthSpec};
use muxa::ipc::{Client, RuntimeError};
use muxa::state::Agent;
use muxa::tmux::{self, PaneInfo};
use muxa::AgentState;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Max time to wait for a single keystroke when the input buffer is
/// empty. ~60 Hz so a press feels immediate without burning CPU on an
/// idle terminal. Held keys / fast typing are absorbed by the
/// drain-all-pending pattern in `run`, so this only governs idle
/// responsiveness.
const INPUT_POLL: Duration = Duration::from_millis(16);

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

/// A single column in the watch TUI. The set of valid columns is fixed by
/// this enum — the `[watch]` config picks which ones to show and in what
/// order, but cannot introduce new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchColumn {
    Pane,
    Kind,
    State,
    Model,
    Ctx,
    Cost,
    Prompt,
    Activity,
}

impl WatchColumn {
    /// Parse a config-string column key. Returns `None` for unknown keys
    /// so the caller can warn and skip rather than refuse to load.
    fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "pane" => Self::Pane,
            "kind" => Self::Kind,
            "state" => Self::State,
            "model" => Self::Model,
            "ctx" => Self::Ctx,
            "cost" => Self::Cost,
            "prompt" => Self::Prompt,
            "activity" => Self::Activity,
            _ => return None,
        })
    }

    fn header(self) -> &'static str {
        match self {
            Self::Pane => "PANE",
            Self::Kind => "KIND",
            Self::State => "STATE",
            Self::Model => "MODEL",
            Self::Ctx => "CTX%",
            Self::Cost => "COST$",
            Self::Prompt => "LAST PROMPT",
            Self::Activity => "ACTIVITY",
        }
    }

    fn default_width(self) -> Constraint {
        match self {
            // PANE — "session:window.pane" can run long; 22 covers most.
            Self::Pane => Constraint::Length(22),
            Self::Kind => Constraint::Length(12),
            Self::State => Constraint::Length(14),
            Self::Model => Constraint::Length(16),
            Self::Ctx => Constraint::Length(5),
            Self::Cost => Constraint::Length(7),
            Self::Prompt => Constraint::Min(20),
            Self::Activity => Constraint::Length(10),
        }
    }

    /// Build the `Text` content for one cell. Returning `Text` (rather
    /// than a finished `Cell`) lets the caller stack a second line on top
    /// of it when the row is selected and a detail template is enabled.
    fn agent_text<'a>(self, a: &'a Agent, now: OffsetDateTime, panes: &'a [PaneInfo]) -> Text<'a> {
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
                let style = match a.state {
                    AgentState::Working => Style::default().fg(Color::Green),
                    AgentState::WaitingInput => Style::default().fg(Color::Yellow),
                    AgentState::Error => Style::default().fg(Color::Red),
                    AgentState::Idle | AgentState::Stopped => Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    AgentState::Starting => Style::default().fg(Color::Cyan),
                };
                Text::from(Span::styled(a.state.to_string(), style))
            }
            Self::Model => a.model.as_deref().unwrap_or("-").to_string().into(),
            Self::Ctx => a
                .context_used_pct
                .map_or_else(|| "-".into(), |p| format!("{p:>3.0}%"))
                .into(),
            Self::Cost => a
                .cost_usd
                .map_or_else(|| "-".into(), |c| format!("${c:.2}"))
                .into(),
            Self::Prompt => a
                .last_prompt
                .as_deref()
                .unwrap_or("-")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>()
                .into(),
            Self::Activity => relative_time(a.last_activity_at, now).into(),
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
            Self::Kind | Self::State => Text::from(Span::styled("—", dim)),
            Self::Model | Self::Ctx | Self::Cost | Self::Activity => {
                Text::from(Span::styled("-", dim))
            }
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
            Self::Model => "model",
            Self::Ctx => "ctx",
            Self::Cost => "cost",
            Self::Prompt => "prompt",
            Self::Activity => "activity",
        }
    }
}

/// One row of the dashboard. Either a tracked muxa agent or a plain tmux
/// pane the daemon doesn't know about — listing both makes `muxa watch` a
/// drop-in replacement for tmux's `choose-tree -Zs`.
pub(crate) enum WatchRow {
    Agent(Agent),
    BarePane(PaneInfo),
}

impl WatchRow {
    fn pane_id(&self) -> Option<&str> {
        match self {
            Self::Agent(a) => a.pane.as_deref(),
            Self::BarePane(p) => Some(&p.pane_id),
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

/// State held by the TUI.
///
/// Kept separate from rendering so the smoke test can construct it
/// directly without touching a real terminal.
pub(crate) struct App {
    pub rows: Vec<WatchRow>,
    pub table_state: TableState,
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
    /// preview (key `p`). The table is hidden behind the preview while
    /// this is set; `q`/`Esc`/`p` clears it.
    pub preview: Option<PreviewState>,
    /// Count of paneless agents that were filtered out of `rows` because
    /// `watch_cfg.hide_paneless` is true. Surfaced as a footer hint so
    /// users know the rows aren't lost — they just aren't actionable from
    /// the picker. Always 0 when `hide_paneless = false`.
    pub paneless_hidden: usize,
    /// Most recent `tmux capture-pane -ep` result, keyed by `pane_id`.
    /// Populated on demand when the preview is in
    /// [`PreviewContent::LivePane`] and re-captured on every refresh
    /// tick while the preview stays open in that mode. `None` when the
    /// preview is closed or showing prompt/response content.
    pub pane_capture: Option<CapturedPane>,
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
        Self::with_config(WatchConfig::default())
    }

    pub(crate) fn with_config(cfg: WatchConfig) -> Self {
        let columns = resolve_columns(&cfg);
        Self {
            rows: Vec::new(),
            table_state: TableState::default(),
            last_error: None,
            last_refresh: OffsetDateTime::now_utc(),
            watch_cfg: cfg,
            columns,
            panes: Vec::new(),
            refresh_pending: false,
            initial_pane: None,
            preview: None,
            paneless_hidden: 0,
            pane_capture: None,
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
    pub(crate) fn set_data(&mut self, mut agents: Vec<Agent>, panes: Vec<PaneInfo>) {
        // Filter out paneless agents up front when the user has opted in
        // (the default). They can't be attached to from the picker — Enter
        // is a no-op — so listing them just clutters the actionable view.
        // The count is preserved on `paneless_hidden` so the footer can
        // surface a `+N paneless` hint and the rows aren't silently lost.
        self.paneless_hidden = 0;
        if self.watch_cfg.hide_paneless {
            let before = agents.len();
            agents.retain(|a| a.pane.is_some());
            self.paneless_hidden = before - agents.len();
        }

        // Sort agent rows according to the user's `[watch] sort` config.
        // Stale agents (pane already closed, i.e. lookup miss against the
        // panes inventory) always bucket at the end so live agents stay
        // visually grouped at the top regardless of the sort keys.
        //
        // Agent records carry only `pane_id`; session / window / pane
        // indices are resolved via the panes inventory collected this
        // refresh.
        let pane_by_id: HashMap<&str, &PaneInfo> =
            panes.iter().map(|p| (p.pane_id.as_str(), p)).collect();
        let sort_keys = &self.watch_cfg.sort;
        agents.sort_by(|a, b| sort_agents(a, b, sort_keys, &pane_by_id));

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
        rows.extend(agents.into_iter().map(WatchRow::Agent));
        rows.extend(bare.into_iter().map(WatchRow::BarePane));

        self.rows = rows;
        // Keep the *full* pane inventory (not just the bare ones) so
        // `pane_display` can resolve `session:window.pane` labels for
        // agent rows by lookup instead of a tmux shell-out per render.
        self.panes = panes;
        self.last_refresh = OffsetDateTime::now_utc();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.table_state.select(None);
            return;
        }
        match self.table_state.selected() {
            Some(i) if i >= self.rows.len() => {
                self.table_state.select(Some(self.rows.len() - 1));
            }
            None => {
                // First non-empty load: prefer the row matching the pane the
                // user invoked muxa from, so the cursor lands on context.
                // `take()` ensures later refreshes don't re-snap selection.
                let hint = self.initial_pane.take();
                let initial = hint
                    .as_deref()
                    .and_then(|id| self.rows.iter().position(|r| r.pane_id() == Some(id)))
                    .unwrap_or(0);
                self.table_state.select(Some(initial));
            }
            Some(_) => {}
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i + 1 < self.rows.len() => i + 1,
            // wrap from the bottom row back to the top; `None` (no prior
            // selection) also lands here and starts at row 0
            Some(_) | None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub(crate) fn move_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i > 0 => i - 1,
            // wrap from the top row back to the bottom
            Some(_) => self.rows.len() - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    /// `pane_id` of the currently selected row, if any.
    pub(crate) fn selected_pane(&self) -> Option<String> {
        let i = self.table_state.selected()?;
        self.rows.get(i)?.pane_id().map(String::from)
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
    pane_by_id: &HashMap<&str, &PaneInfo>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let info_a = a.pane.as_deref().and_then(|id| pane_by_id.get(id).copied());
    let info_b = b.pane.as_deref().and_then(|id| pane_by_id.get(id).copied());

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

/// One full result of a refresh — what the background task hands back to
/// the main loop on every tick or wake. The main task holds nothing else
/// daemon-related, so all the state diffs come through this single struct.
pub(crate) struct RefreshOutcome {
    pub agents: Vec<Agent>,
    pub panes: Vec<PaneInfo>,
    pub error: Option<DaemonError>,
}

/// Apply a `RefreshOutcome` to `App` exactly the way the old inline
/// `refresh` helper did. Kept as a free function so unit tests can build
/// outcomes from a fake fetcher and assert on `App` afterwards without
/// pulling in any networking.
pub(crate) fn apply_outcome(app: &mut App, outcome: RefreshOutcome) {
    app.last_error = outcome.error;
    app.set_data(outcome.agents, outcome.panes);
}

/// Compute one refresh outcome: tmux pane inventory (off-runtime via
/// `spawn_blocking`) plus a daemon snapshot. Kept independent of `App` so
/// the work can run on a worker thread without holding any UI state.
async fn compute_refresh(client: &Client) -> RefreshOutcome {
    // tmux pane inventory is independent of the daemon — fetch it even
    // when muxad is down so `muxa watch` stays useful as a session picker.
    // `tmux::list_panes` shells out (~few ms) and must NOT run on a tokio
    // worker — that's the whole point of this refactor.
    let panes = tokio::task::spawn_blocking(tmux::list_panes)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();

    match client.snapshot().await {
        Ok(agents) => RefreshOutcome {
            agents,
            panes,
            error: None,
        },
        Err(e) => RefreshOutcome {
            agents: Vec::new(),
            panes,
            error: Some(DaemonError {
                self_describing: matches!(e, RuntimeError::NotConnected(_)),
                message: e.to_string(),
            }),
        },
    }
}

/// Background task that owns its own `Client` clone and produces refresh
/// outcomes on a 500 ms tick or whenever the input loop sends a wake
/// request. The task exits cleanly when *either* end of either channel
/// closes — main drops `wake_tx` to signal shutdown.
///
/// Generic over the fetcher so unit tests can swap in a closure that
/// returns a canned `RefreshOutcome` without touching tmux or the daemon.
async fn refresh_task<F, Fut>(
    mut fetch: F,
    mut wake: mpsc::Receiver<()>,
    out: mpsc::Sender<RefreshOutcome>,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = RefreshOutcome> + Send,
{
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    // If a refresh runs longer than one tick (slow daemon, slow tmux),
    // don't pile up backlog ticks — skip them.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first `tick()` fires immediately. We don't want a duplicate
    // refresh right after the priming snapshot in `run`, so consume it.
    tick.tick().await;

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            req = wake.recv() => {
                // None => the input loop dropped wake_tx, i.e. quit/attach.
                if req.is_none() {
                    return;
                }
            }
        }
        let outcome = fetch().await;
        if out.send(outcome).await.is_err() {
            // Main loop dropped its receiver (quit/attach) — go home.
            return;
        }
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
pub async fn run(client: &Client, watch_cfg: WatchConfig) -> Result<Option<String>> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);

    let mut app = App::with_config(watch_cfg);
    // When invoked from inside tmux, land the cursor on the user's current
    // pane on first load instead of always row 0.
    app.set_initial_pane(tmux::current_pane());

    // Prime the initial snapshot so the first frame already has data —
    // otherwise the user sees an empty table for ~one tick.
    apply_outcome(&mut app, compute_refresh(client).await);

    // Background refresh task owns its own Client clone so the borrowed
    // `client: &Client` doesn't have to outlive the task. The clone is
    // cheap (a single `PathBuf`) and avoids needing an `Arc`/lifetime
    // wrapper for what is effectively immutable data.
    let bg_client = client.clone();
    let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    let (outcome_tx, mut outcome_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);
    let bg = tokio::spawn(refresh_task(
        move || {
            let client = bg_client.clone();
            async move { compute_refresh(&client).await }
        },
        wake_rx,
        outcome_tx,
    ));

    let mut jump_target: Option<String> = None;

    loop {
        guard
            .terminal_mut()
            .draw(|f| render(f, &mut app))
            .map_err(anyhow::Error::from)?;

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

        let mut quit = false;
        for ev in events {
            match handle_event(ev, &mut app) {
                Action::Quit => {
                    quit = true;
                    break;
                }
                Action::Attach => {
                    if let Some(pane) = app.selected_pane() {
                        jump_target = Some(pane);
                        quit = true;
                        break;
                    }
                }
                Action::Refresh => {
                    // Coalesce repeated `r` mashes: if the wake slot is
                    // already full a request is pending, so a `try_send`
                    // failure is fine — the in-flight request will pick
                    // up the user's intent.
                    let _ = wake_tx.try_send(());
                    app.refresh_pending = true;
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
                Action::None => {}
            }
        }

        // Live pane capture: when the preview is open in LivePane
        // mode and the cache is missing or stale (>500 ms), shell out
        // to `tmux capture-pane -ep -t <pane>` on a worker thread.
        // Bounded by the existing 500 ms TTL so we never fork more
        // than ~2 Hz, regardless of how fast the input loop spins.
        if let Some(p) = &app.preview {
            if p.content == PreviewContent::LivePane {
                let stale = app
                    .pane_capture
                    .as_ref()
                    .is_none_or(|c| {
                        c.pane_id != p.pane_id
                            || c.fetched_at.elapsed() >= Duration::from_millis(500)
                    });
                if stale {
                    let pane_id = p.pane_id.clone();
                    let captured = tokio::task::spawn_blocking(move || {
                        muxa::tmux::capture_pane(&pane_id)
                    })
                    .await
                    .ok()
                    .and_then(Result::ok);
                    app.pane_capture = Some(CapturedPane {
                        pane_id: p.pane_id.clone(),
                        text: captured.unwrap_or_default(),
                        fetched_at: std::time::Instant::now(),
                    });
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
        }

        if quit {
            break;
        }
    }

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

enum Action {
    None,
    Quit,
    Refresh,
    /// Attach to the currently-selected pane.
    Attach,
    /// Pop open the preview overlay for the selected row.
    OpenPreview,
    /// Close the preview overlay and return to the table.
    ClosePreview,
    /// Swap the preview between popup and full-screen modes.
    TogglePreviewMode,
    /// Swap the preview content between prompt/response and live pane
    /// capture. Composes with `TogglePreviewMode` (geometry).
    TogglePreviewContent,
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

    // Ctrl-C is global — quits regardless of mode.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        return Action::Quit;
    }

    // Preview mode: arrow keys scroll the overlay instead of moving the
    // table cursor; quit/back collapses the overlay rather than the app.
    // We mutate `app.preview` inline (mirroring how table-mode arrows
    // mutate `table_state` directly) so the run loop only has to handle
    // open/close transitions.
    if let Some(preview) = app.preview.as_mut() {
        return match code {
            KeyCode::Char('q' | 'p') | KeyCode::Esc => Action::ClosePreview,
            KeyCode::Char('f') => Action::TogglePreviewMode,
            KeyCode::Char('c') => Action::TogglePreviewContent,
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Down | KeyCode::Char('j') => {
                preview.scroll = preview.scroll.saturating_add(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                preview.scroll = preview.scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::PageDown => {
                preview.scroll = preview.scroll.saturating_add(10);
                Action::None
            }
            KeyCode::PageUp => {
                preview.scroll = preview.scroll.saturating_sub(10);
                Action::None
            }
            KeyCode::Home => {
                preview.scroll = 0;
                Action::None
            }
            _ => Action::None,
        };
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Enter => Action::Attach,
        KeyCode::Char('p') => Action::OpenPreview,
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
    match app.preview.as_ref().map(|p| p.mode) {
        Some(PreviewMode::Fullscreen) => {
            render_preview(f, chunks[1], app);
        }
        Some(PreviewMode::Popup) => {
            // Render the table behind so the user keeps a sense of
            // "where am I in the list" — then `Clear` the popup area
            // (wipes the cells under it so the popup paints clean) and
            // render the preview on top.
            render_table(f, chunks[1], app);
            let popup_area = centered_rect(80, 70, chunks[1]);
            f.render_widget(Clear, popup_area);
            render_preview(f, popup_area, app);
        }
        None => {
            render_table(f, chunks[1], app);
        }
    }
    render_footer(f, chunks[2], app);
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

/// Full-screen detail view for the agent / pane the user pinned with `p`.
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
    let preview = app
        .preview
        .as_ref()
        .expect("render_preview without preview");

    let mode_tag = match preview.content {
        PreviewContent::PromptResponse => "prompt",
        PreviewContent::LivePane => "live",
    };
    let title = format!(" preview · {} · {} ", preview.pane_id, mode_tag);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

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

    let row = app
        .rows
        .iter()
        .find(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some(pane_id)));

    let pane_label = pane_display(Some(pane_id), &app.panes);
    if let Some(WatchRow::Agent(agent)) = row {
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
            "(press q / Esc / p to return to the picker)",
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

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let agents = app
        .rows
        .iter()
        .filter(|r| matches!(r, WatchRow::Agent(_)))
        .count();
    let bare = app.rows.len() - agents;
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
            format!("{agents} agent{}", if agents == 1 { "" } else { "s" }),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("+ {bare} pane{}", if bare == 1 { "" } else { "s" }),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled(clock, Style::default().fg(Color::DarkGray)),
    ]);
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

    let err_line = app
        .last_error
        .as_ref()
        .map(|e| {
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
    let header_cells = app.columns.iter().map(|c| {
        Cell::from(c.header()).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header = Row::new(header_cells).height(1);

    let now = OffsetDateTime::now_utc();
    let selected = app.table_state.selected();
    let detail_host = detail_host_column(&app.columns);
    let rows: Vec<Row> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut texts: Vec<Text> = match r {
                WatchRow::Agent(a) => app
                    .columns
                    .iter()
                    .map(|c| c.agent_text(a, now, &app.panes))
                    .collect(),
                WatchRow::BarePane(p) => app.columns.iter().map(|c| c.bare_text(p)).collect(),
            };

            let mut expanded = false;
            if Some(i) == selected && app.watch_cfg.detail.enabled {
                if let Some(host) = detail_host {
                    if let Some(detail) =
                        format_detail(&app.watch_cfg.detail.template, r, &app.panes, now)
                    {
                        texts[host] = stack_detail(std::mem::take(&mut texts[host]), &detail);
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
            "state" | "model" | "ctx" | "cost" | "activity" | "last_response"
            | "last_notification" | "cwd" => "—".into(),
            _ => return None,
        }),
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

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    // Preview mode rebinds the table-mode keybinds to their preview-pane
    // analogues — clearer for the user than leaving the same hint strings
    // up while the keys behave differently. The `f` hint label flips with
    // the mode so the user knows what tapping it will do.
    if let Some(preview) = app.preview.as_ref() {
        let toggle_label = match preview.mode {
            PreviewMode::Popup => " fullscreen  ",
            PreviewMode::Fullscreen => " popup  ",
        };
        // Content toggle reads as the *target* state (where `c` would
        // take you) so the hint stays actionable rather than describing
        // where you already are.
        let content_label = match preview.content {
            PreviewContent::PromptResponse => " live pane  ",
            PreviewContent::LivePane => " prompt  ",
        };
        let spans = vec![
            Span::styled(" ↑/↓ ", Style::default().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" scroll  "),
            Span::styled(
                " PgUp/PgDn ",
                Style::default().fg(Color::Black).bg(Color::Gray),
            ),
            Span::raw(" page  "),
            Span::styled(" f ", Style::default().fg(Color::Black).bg(Color::Gray)),
            Span::raw(toggle_label),
            Span::styled(" c ", Style::default().fg(Color::Black).bg(Color::Gray)),
            Span::raw(content_label),
            Span::styled(" r ", Style::default().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" refresh  "),
            Span::styled(
                " p/q/Esc ",
                Style::default().fg(Color::Black).bg(Color::Gray),
            ),
            Span::raw(" back"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let mut spans = vec![
        Span::styled(" ↑/↓ ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" move  "),
        Span::styled(" ⏎ ", Style::default().fg(Color::Black).bg(Color::Green)),
        Span::raw(" attach  "),
        Span::styled(" p ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" preview  "),
        Span::styled(" r ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" refresh  "),
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit"),
    ];
    // When the highlighted row has no pane to attach to (e.g. a Claude
    // SDK sub-process whose env didn't carry TMUX_PANE and whose
    // ancestry walk didn't recover one) tell the user why Enter is a
    // no-op rather than letting the keystroke vanish silently.
    if selected_has_no_pane(app) {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            "no tmux pane — attach unavailable",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
    }
    // When paneless agents were hidden by config, surface the count so
    // they remain discoverable. `--include-paneless` (or
    // `[watch] hide_paneless = false`) brings them back.
    if app.paneless_hidden > 0 {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            format!(
                "+{} paneless (use --include-paneless to show)",
                app.paneless_hidden
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn selected_has_no_pane(app: &App) -> bool {
    let Some(i) = app.table_state.selected() else {
        return false;
    };
    match app.rows.get(i) {
        Some(WatchRow::Agent(a)) => a.pane.is_none(),
        // BarePane rows always have a pane id; tmux gives them a
        // pane_id by definition.
        Some(WatchRow::BarePane(_)) | None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::event::{AgentKind, AgentState};
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;
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
            last_response: None,
            last_notification: None,
            model: model.map(Into::into),
            context_used_pct: ctx,
            cost_usd: cost,
            started_at: now,
            last_activity_at: now,
        }
    }

    fn fake_pane(pane: &str, session: &str, window: u32, pane_idx: u32, cmd: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane.into(),
            session: session.into(),
            window_index: window.to_string(),
            pane_index: pane_idx.to_string(),
            tty: "/dev/pts/0".into(),
            current_command: cmd.into(),
            title: cmd.into(),
        }
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
                _ => None,
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
                _ => None,
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
                _ => None,
            })
            .collect();
        // alpha: %11 (newer t2) before %10 (older t0); then beta: %21
        // (newer t3) before %20 (older t1).
        assert_eq!(order, vec!["%11", "%10", "%21", "%20"]);
    }

    #[test]
    fn activity_only_sort_floats_globally_newest_agent_to_the_top() {
        // sort = [Activity] — drops session grouping entirely. Expected
        // order is strict newest-first across all sessions.
        let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
        let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
        let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);

        let cfg = WatchConfig {
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
                _ => None,
            })
            .collect();
        assert_eq!(order, vec!["%20", "%30", "%10"]);
    }

    #[test]
    fn pane_id_sort_produces_lexicographic_order() {
        // sort = [PaneId] — useful for screenshots / docs where stable
        // alphabetic order is preferred over recency.
        let now = OffsetDateTime::now_utc();
        let cfg = WatchConfig {
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
                _ => None,
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
        let older = now - time::Duration::hours(1);

        let cfg = WatchConfig {
            sort: vec![WatchSortKey::Activity],
            ..WatchConfig::default()
        };
        let mut app = App::with_config(cfg);
        app.set_data(
            vec![
                fake_agent_at("stale-but-recent", "%999", very_recent),
                fake_agent_at("live-but-older", "%10", older),
            ],
            vec![fake_pane("%10", "main", 0, 0, "claude")],
        );

        let order: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.as_deref(),
                _ => None,
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
            pane_id: "%42".into(),
            session: "main".into(),
            window_index: "1".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: String::new(),
            title: String::new(),
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
            dump.contains("> %25"),
            "expected the highlighted '%25' row to be scrolled into view, got:\n{dump}",
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
        assert!(relative_time(now, now).ends_with("s ago"));
        assert!(relative_time(now - time::Duration::minutes(5), now).ends_with("m ago"));
        assert!(relative_time(now - time::Duration::hours(3), now).ends_with("h ago"));
        assert!(relative_time(now - time::Duration::days(3), now).ends_with("d ago"));
    }

    #[test]
    fn default_columns_are_prompt_forward() {
        let app = App::new();
        assert_eq!(
            app.columns,
            vec![
                WatchColumn::Pane,
                WatchColumn::State,
                WatchColumn::Prompt,
                WatchColumn::Activity,
            ]
        );
    }

    #[test]
    fn custom_columns_resolve_in_config_order() {
        let cfg = WatchConfig {
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
            WatchColumn::Prompt,
            WatchColumn::Activity,
        ] {
            let _ = col.agent_text(&a, now, &[]);
        }
    }

    #[test]
    fn bare_pane_summary_lands_in_prompt_column() {
        // Render with a column set that includes Prompt but excludes the
        // others — we verify the BarePane row is built without panic and
        // that the prompt column carries the summary.
        let cfg = WatchConfig {
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
        RefreshOutcome {
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
            error: None,
        }
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
        ));

        // The 500 ms tick is paused; force the wake path.
        wake_tx.try_send(()).expect("wake slot empty at start");
        let outcome = out_rx.recv().await.expect("refresh outcome on wake");
        assert_eq!(outcome.agents.len(), 1);
        assert_eq!(outcome.agents[0].session_id, "call-0");
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
        ));

        // Time is paused; advance past one full POLL_INTERVAL so the
        // interval fires its second tick (the first is consumed inside
        // refresh_task before the loop).
        tokio::time::advance(POLL_INTERVAL + Duration::from_millis(50)).await;
        let outcome = out_rx.recv().await.expect("refresh outcome on tick");
        assert_eq!(outcome.agents[0].session_id, "tick");
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
        let s = format_detail("{last_response}", &row, &[], now).unwrap();
        assert_eq!(s, "here is what I did");
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
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
    fn detail_disabled_skips_expansion() {
        let cfg = WatchConfig {
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
        // Existing Action::Attach branch is `if let Some(pane) = ...`,
        // so returning None here is what causes Enter to silently no-op.
        // The footer hint added above is the user-facing fix; this
        // assertion just nails down the underlying contract.
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
        let outcome = RefreshOutcome {
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
            error: Some(DaemonError {
                self_describing: false,
                message: "boom".into(),
            }),
        };
        apply_outcome(&mut app, outcome);
        assert_eq!(app.rows.len(), 2);
        assert!(app.last_error.is_some());
        assert_eq!(app.last_error.as_ref().unwrap().message, "boom");
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::Agent(a);
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
        let row = WatchRow::BarePane(p);
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
        let row = WatchRow::BarePane(p);
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

    /// Press a key and apply the resulting `Action` to `app` the same
    /// way the main run loop does. Mirrors the dispatch table in
    /// `watch::run` so tests can read as "press X, expect Y" without
    /// inlining the open/close/toggle book-keeping every time.
    fn press(app: &mut App, c: char) {
        match key_action(app, c) {
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
            Action::None | Action::Quit | Action::Refresh | Action::Attach => {}
        }
    }

    #[test]
    fn preview_opens_with_p_and_pins_selected_pane_id() {
        let mut app = three_agent_app(muxa::config::DetailConfig::default());
        app.table_state.select(Some(1)); // %2

        assert!(app.preview.is_none());
        let action = key_action(&mut app, 'p');
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
    fn preview_closes_with_q_esc_or_p() {
        for key in [KeyCode::Char('q'), KeyCode::Esc, KeyCode::Char('p')] {
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
        let action = key_action(&mut app, 'p');
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

    /// `c` toggles the preview content axis: `PromptResponse` → `LivePane`
    /// → `PromptResponse`. Geometry mode (popup vs fullscreen) is unaffected
    /// — the two axes compose. Scroll resets so the new content surface
    /// starts at the top instead of mid-line.
    /// Overlay preset that opens to PromptResponse — used by tests that
    /// want to pin the starting content axis instead of inheriting whatever
    /// the global default happens to be. Keeps test intent stable across
    /// future default flips.
    fn cfg_with_prompt_default() -> WatchConfig {
        WatchConfig {
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
}
