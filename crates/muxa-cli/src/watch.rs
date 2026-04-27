//! `muxa watch` — fullscreen ratatui dashboard.
//!
//! Polls the daemon via `Client::snapshot()` every 500 ms and renders a
//! live-updating table of tracked agents. Input is handled via crossterm
//! events (`q`/`Esc`/`Ctrl-C` to quit, `r` to force-refresh, `↑/↓` or
//! `j/k` for selection, `Enter` to attach into the selected pane).
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
use muxa::config::{WatchConfig, WidthSpec};
use muxa::ipc::{Client, RuntimeError};
use muxa::state::Agent;
use muxa::tmux::{self, PaneInfo};
use muxa::AgentState;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use std::collections::HashSet;
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

    fn agent_cell<'a>(self, a: &'a Agent, now: OffsetDateTime, panes: &'a [PaneInfo]) -> Cell<'a> {
        match self {
            Self::Pane => Cell::from(pane_display(a.pane.as_deref(), panes)),
            Self::Kind => Cell::from(a.kind.to_string()),
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
                Cell::from(a.state.to_string()).style(style)
            }
            Self::Model => Cell::from(a.model.as_deref().unwrap_or("-").to_string()),
            Self::Ctx => Cell::from(
                a.context_used_pct
                    .map_or_else(|| "-".into(), |p| format!("{p:>3.0}%")),
            ),
            Self::Cost => Cell::from(
                a.cost_usd
                    .map_or_else(|| "-".into(), |c| format!("${c:.2}")),
            ),
            Self::Prompt => Cell::from(
                a.last_prompt
                    .as_deref()
                    .unwrap_or("-")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>(),
            ),
            Self::Activity => Cell::from(relative_time(a.last_activity_at, now)),
        }
    }

    fn bare_cell(self, p: &PaneInfo) -> Cell<'_> {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        match self {
            Self::Pane => {
                let pane = format!("{}:{}.{}", p.session, p.window_index, p.pane_index);
                Cell::from(pane).style(dim)
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
                Cell::from(summary).style(dim)
            }
            Self::Kind | Self::State => Cell::from("—").style(dim),
            Self::Model | Self::Ctx | Self::Cost | Self::Activity => Cell::from("-").style(dim),
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
        }
    }

    /// Replace the row set. `agents` (tracked) are listed first in stable
    /// order; `panes` minus any pane already represented by an agent are
    /// appended as `BarePane` rows.
    pub(crate) fn set_data(&mut self, mut agents: Vec<Agent>, panes: Vec<PaneInfo>) {
        agents.sort_by(|a, b| {
            a.pane
                .as_deref()
                .unwrap_or("")
                .cmp(b.pane.as_deref().unwrap_or(""))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

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
            None => self.table_state.select(Some(0)),
            Some(_) => {}
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i + 1 < self.rows.len() => i + 1,
            Some(_) => self.rows.len() - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub(crate) fn move_up(&mut self) {
        if self.rows.is_empty() {
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
        self.rows.get(i)?.pane_id().map(String::from)
    }
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
        return "-".into();
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
pub async fn run(client: &Client, watch_cfg: WatchConfig) -> Result<Option<String>> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);

    let mut app = App::with_config(watch_cfg);

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
                Action::None => {}
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
    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|r| match r {
            WatchRow::Agent(a) => Row::new(
                app.columns
                    .iter()
                    .map(|c| c.agent_cell(a, now, &app.panes))
                    .collect::<Vec<_>>(),
            ),
            WatchRow::BarePane(p) => Row::new(
                app.columns
                    .iter()
                    .map(|c| c.bare_cell(p))
                    .collect::<Vec<_>>(),
            ),
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
        assert_eq!(pane_display(None, &panes), "-");
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
    fn selection_movement_is_bounded() {
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
    fn agent_cell_renders_for_each_column_kind() {
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
        // Smoke-test that every column variant produces a Cell without panic.
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
            let _ = col.agent_cell(&a, now, &[]);
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
}
