//! `muxa peek` — tmux's `display-panes`, carrying muxa's per-pane context.
//!
//! `display-panes` answers "which pane is which number". This answers
//! "which pane is doing what": over each pane's own screen sits a box with
//! its agent's state glyph, session summary, latest prompt, and latest
//! response. Typing the pane's digit jumps there, so it takes over
//! `prefix + q` outright rather than asking for a modifier to reach the
//! better version of a reflex you already have.
//!
//! ## Faking transparency
//!
//! tmux allows exactly one popup per client, offers no per-pane overlay
//! primitive, and its popups are opaque. So "a translucent card floating
//! over each pane" is not expressible — but it can be *reconstructed*: one
//! borderless popup covers the whole client, and inside it we repaint the
//! window's pane layout from `#{pane_left}`/`#{pane_top}` (see
//! [`muxa::tmux::layout`]), fill each rectangle with that pane's captured
//! screen dimmed to a backdrop, and lay the info box on top.
//!
//! Two rules keep the illusion honest. The box takes only the rows its
//! content needs, never more than two thirds of the pane, so the terminal
//! underneath stays readable — a box that filled its pane would be an
//! opaque overlay again. And the box's own rectangle is cleared before
//! drawing, because backdrop text bleeding through the box's interior
//! reads as corruption rather than depth.
//!
//! ## Reading the focused pane
//!
//! Inside a popup `$TMUX_PANE` names the *popup's own* pane, so
//! [`muxa::tmux::current_pane`] cannot answer "which pane is the user on".
//! Focus comes from tmux's `#{pane_active}` instead, carried on
//! [`PaneGeometry::active`].

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::config::IconSet;
use muxa::ipc::Client;
use muxa::state::Agent;
use muxa::tmux::layout::{PaneGeometry, WindowFrame, WindowTarget};
use muxa::AgentState;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthChar;

use crate::attend;
use crate::watch::agent_kind_short;

/// Budget for the one snapshot round-trip. The overlay is a
/// press-and-glance affair, so a wedged daemon must degrade to "boxes
/// with no agent detail" fast rather than leaving the user staring at a
/// blank popup. Looser than the status-line's 250 ms because this fires
/// once per keypress, not twice per second.
const PEEK_IPC_TIMEOUT: Duration = Duration::from_millis(400);

/// How often the overlay re-reads agents and pane geometry while open.
/// Peek stays up until dismissed (unlike `display-panes`, whose content
/// is static enough to time out), so a state flip or a layout change made
/// from another client should show up without a manual refresh. Pane
/// backdrops are deliberately *not* on this tick — see the refresh arm in
/// [`drive`].
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Input poll slice. Short enough that a digit feels instant, long enough
/// that idling costs nothing.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How far back to look in a pane's prompt history for an entry
/// belonging to the current agent session. History is newest-first, so
/// the match is normally the first entry; the small slack covers a pane
/// whose previous occupant's prompts have not aged out yet.
const PROMPT_HISTORY_LOOKBACK: usize = 8;

/// Smallest box that can carry a border plus one row of content. Below
/// this, [`render_cell`] drops the border and prints a bare badge.
const MIN_BORDERED_HEIGHT: u16 = 3;

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct Args {
    /// Print the overlay's per-pane lines as plain text and exit, instead
    /// of drawing the TUI. Intended for `muxa doctor`-style debugging from
    /// a normal shell, where there's no popup to draw into.
    #[arg(long)]
    plain: bool,
}

/// One pane's worth of overlay: where to draw, and what to say.
#[derive(Debug, Clone)]
pub(crate) struct PeekCell {
    pub geo: PaneGeometry,
    /// The agent whose story this box tells. `None` for a pane running a
    /// plain shell, which gets a bare digit badge instead of a box —
    /// nothing to narrate, and its output stays fully visible.
    pub agent: Option<Agent>,
    /// Agents sharing this pane beyond `agent` (a restarted session that
    /// hasn't been reaped yet, or a `muxa register`ed task). Surfaced as a
    /// `+N` badge rather than silently dropped.
    pub extra: usize,
    /// When the human last sent this agent a prompt, from the daemon's
    /// prompt history. `None` when history holds nothing for this
    /// agent's session — a fresh session, or history switched off — in
    /// which case the prompt line simply carries no age.
    pub last_prompt_at: Option<OffsetDateTime>,
    /// The pane's visible screen, one entry per row, painted dim behind
    /// the info box. Taken once when the overlay opens and re-taken only
    /// on `r`/resize, so it is a snapshot rather than a live mirror.
    /// Empty when the capture failed or wasn't requested (`--plain`,
    /// tests) — the box then renders over blank space.
    pub capture: Vec<String>,
}

pub(crate) async fn run(client: &Client, args: Args) -> Result<()> {
    // Pin the window before anything else: every later query is scoped to
    // it, so a keystroke in another terminal can't reroute the overlay
    // onto a different session mid-read.
    let target = WindowTarget::resolve();
    let (panes, zoomed) = muxa::tmux::layout::current_window_panes(&target);
    if panes.is_empty() {
        anyhow::bail!(
            "no tmux panes visible — `muxa peek` reads the current window, so run it inside tmux \
             (normally via `prefix + q`)"
        );
    }
    let frame = muxa::tmux::layout::current_window_frame(&target);
    let agents = client
        .snapshot_with_timeout(PEEK_IPC_TIMEOUT)
        .await
        .unwrap_or_default();
    let mut cells = build_cells(panes, &agents);
    attach_prompt_times(client, &mut cells).await;

    if args.plain {
        for line in plain_lines(&cells) {
            println!("{line}");
        }
        return Ok(());
    }
    attach_captures(&mut cells, zoomed);

    // The guard restores the terminal on the way out of every path,
    // including a panic mid-draw. Outside a popup (`muxa peek` run bare in
    // a shell) nothing else would put the terminal back.
    let mut guard = TerminalGuard::new(setup_terminal()?);
    let outcome = drive(guard.terminal_mut(), client, cells, frame, zoomed, &target).await;
    drop(guard);
    // Jump only after the popup's screen is torn down: `select-pane`
    // repaints the window underneath, and doing it while we still own the
    // alternate screen leaves the user looking at our leftovers.
    if let Outcome::Jump(pane_id) = outcome? {
        muxa::tmux::tmux_command()
            .args(["select-pane", "-t", &pane_id])
            .status()
            .ok();
    }
    Ok(())
}

enum Outcome {
    Jump(String),
    Dismissed,
}

async fn drive(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &Client,
    mut cells: Vec<PeekCell>,
    frame: Option<WindowFrame>,
    mut zoomed: bool,
    target: &WindowTarget,
) -> Result<Outcome> {
    let placement = Placement::from(frame);
    let mut typed = String::new();
    let mut last_refresh = Instant::now();
    // Set when something invalidated the current frame (an explicit `r`, a
    // resize) so the next pass re-reads immediately instead of waiting out
    // the interval.
    let mut stale = false;
    loop {
        terminal.draw(|f| draw(f, &cells, placement, zoomed, &typed))?;

        if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match classify(key) {
                    Action::Dismiss => return Ok(Outcome::Dismissed),
                    Action::Refresh => {
                        typed.clear();
                        stale = true;
                    }
                    Action::Digit(d) => {
                        typed.push(d);
                        match resolve_typed(&typed, &cells) {
                            Selection::Hit(pane_id) => return Ok(Outcome::Jump(pane_id)),
                            Selection::Prefix => {}
                            Selection::Miss => typed.clear(),
                        }
                    }
                    Action::Commit => {
                        if let Some(pane_id) = exact_match(&typed, &cells) {
                            return Ok(Outcome::Jump(pane_id));
                        }
                        typed.clear();
                    }
                    Action::Ignore => {}
                },
                // A resize invalidates every rectangle we hold; re-read
                // geometry rather than repainting a stale layout.
                Event::Resize(_, _) => stale = true,
                _ => {}
            }
        }

        if stale || last_refresh.elapsed() >= REFRESH_INTERVAL {
            let (panes, now_zoomed) = muxa::tmux::layout::current_window_panes(target);
            if !panes.is_empty() {
                let agents = client
                    .snapshot_with_timeout(PEEK_IPC_TIMEOUT)
                    .await
                    .unwrap_or_default();
                let mut next = build_cells(panes, &agents);
                attach_prompt_times(client, &mut next).await;
                // Agent state is cheap to re-read and worth keeping live.
                // Backdrops are neither: one `capture-pane` per pane per
                // second buys a flicker of scrollback nobody is reading
                // while the overlay is up. They refresh when the user asks
                // (`r`) or when a resize reflows them.
                if stale {
                    attach_captures(&mut next, now_zoomed);
                } else {
                    carry_captures(&cells, &mut next);
                }
                cells = next;
                zoomed = now_zoomed;
            }
            last_refresh = Instant::now();
            stale = false;
        }
    }
}

enum Action {
    Digit(char),
    /// Commit whatever digits are pending, ambiguity be damned.
    Commit,
    Refresh,
    Dismiss,
    Ignore,
}

/// What the digits typed so far mean.
enum Selection {
    Hit(String),
    /// Still ambiguous — `1` when both pane 1 and pane 10 exist. Waiting
    /// for another digit is the only way to tell them apart.
    Prefix,
    Miss,
}

/// Resolve typed digits against the visible pane indexes.
///
/// Single-digit windows (nearly all of them) jump on the first keypress.
/// A window with ten or more panes makes `1` a prefix of `10`, so those
/// wait for a second digit rather than leaving panes 10+ unreachable.
///
/// The wait is why `Enter` exists: with panes 1, 10 and 11 on screen,
/// every continuation of `1` names a *different* pane, so pane 1 could
/// never be reached by typing alone. `Enter` commits what's pending —
/// see [`exact_match`].
fn resolve_typed(typed: &str, cells: &[PeekCell]) -> Selection {
    let exact = cells.iter().find(|c| c.geo.pane_index == typed);
    let ambiguous = cells
        .iter()
        .any(|c| c.geo.pane_index.len() > typed.len() && c.geo.pane_index.starts_with(typed));
    match (exact, ambiguous) {
        (Some(cell), false) => Selection::Hit(cell.geo.pane_id.clone()),
        (_, true) => Selection::Prefix,
        (None, false) => Selection::Miss,
    }
}

/// The pane whose index is exactly `typed`, ignoring the ambiguity that
/// makes [`resolve_typed`] wait. This is what `Enter` commits.
fn exact_match(typed: &str, cells: &[PeekCell]) -> Option<String> {
    cells
        .iter()
        .find(|c| c.geo.pane_index == typed)
        .map(|c| c.geo.pane_id.clone())
}

fn classify(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c' | 'C') => Action::Dismiss,
            _ => Action::Ignore,
        };
    }
    match key.code {
        // `q` mirrors tmux's own `display-panes` dismissal; Esc is the
        // reflex for anything popup-shaped.
        // `Q` too: the key that opened the overlay should close it.
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Action::Dismiss,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Enter => Action::Commit,
        KeyCode::Char(c) if c.is_ascii_digit() => Action::Digit(c),
        _ => Action::Ignore,
    }
}

/// Pair pane geometry with the agent rows the daemon holds for it.
///
/// Every visible pane gets a cell, including the ones a zoomed pane is
/// currently covering: the attention count in the footer must still see
/// them, and `--plain` lists them. Hiding the covered ones is a *drawing*
/// concern, handled in [`draw`].
pub(crate) fn build_cells(panes: Vec<PaneGeometry>, agents: &[Agent]) -> Vec<PeekCell> {
    let here = muxa::tmux::layout::current_socket_name();
    panes
        .into_iter()
        .map(|geo| {
            let mut mine: Vec<&Agent> = agents
                .iter()
                .filter(|a| a.pane.as_deref() == Some(geo.pane_id.as_str()))
                .filter(|a| on_this_server(a, here.as_deref()))
                .collect();
            // Most interesting first: a pane holding both a live agent and
            // the husk of a previous one should read as the live one.
            mine.sort_by(|a, b| {
                interest_rank(a)
                    .cmp(&interest_rank(b))
                    .then(b.last_activity_at.cmp(&a.last_activity_at))
            });
            let extra = mine.len().saturating_sub(1);
            PeekCell {
                agent: mine.first().map(|a| (*a).clone()),
                extra,
                last_prompt_at: None,
                capture: Vec::new(),
                geo,
            }
        })
        .collect()
}

/// Read each visible pane's screen into its cell.
///
/// One `capture-pane` shell-out per pane per refresh. Panes a zoomed pane
/// covers are skipped — they are not drawn, so capturing them would spend
/// a subprocess on pixels nobody sees. A failed capture leaves the cell's
/// backdrop empty rather than failing the frame.
fn attach_captures(cells: &mut [PeekCell], zoomed: bool) {
    for cell in cells.iter_mut() {
        if zoomed && !cell.geo.active {
            cell.capture.clear();
            continue;
        }
        cell.capture = muxa::tmux::layout::capture_pane_plain(&cell.geo.pane_id)
            .map(|raw| raw.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }
}

/// Stamp each cell with when its agent last received a prompt.
///
/// One in-memory history read per pane, matched on `session_id`: a pane
/// that has been reused by a *new* agent session must not inherit the
/// previous occupant's timestamp, and a `muxa register`ed task whose
/// `last_prompt` is really its command line has no history of its own to
/// borrow from.
async fn attach_prompt_times(client: &Client, cells: &mut [PeekCell]) {
    // One deadline for the whole pass, not per pane: a wedged daemon
    // would otherwise freeze the redraw for `panes × timeout`, which on a
    // ten-pane window is four seconds of dead overlay.
    let started = Instant::now();
    for cell in cells.iter_mut() {
        if started.elapsed() >= PEEK_IPC_TIMEOUT {
            break;
        }
        let Some(session_id) = cell.agent.as_ref().map(|a| a.session_id.clone()) else {
            continue;
        };
        let history = client
            .recent_prompts_with_timeout(
                Some(&cell.geo.pane_id),
                Some(PROMPT_HISTORY_LOOKBACK),
                PEEK_IPC_TIMEOUT.saturating_sub(started.elapsed()),
            )
            .await
            .unwrap_or_default();
        cell.last_prompt_at = history
            .into_iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.at);
    }
}

/// Move backdrops from the previous cells onto freshly built ones,
/// matched by pane id. A pane that appeared since the last capture simply
/// has no backdrop until the next `r`.
fn carry_captures(previous: &[PeekCell], next: &mut [PeekCell]) {
    for cell in next.iter_mut() {
        if let Some(old) = previous
            .iter()
            .find(|c| c.geo.pane_id == cell.geo.pane_id)
            .filter(|c| !c.capture.is_empty())
        {
            cell.capture.clone_from(&old.capture);
        }
    }
}

/// Whether an agent row can belong to the tmux server peek is running on.
///
/// Pane ids are only unique *per server*, so with two tmux servers up, a
/// `%5` recorded against one can collide with `%5` on the other and put a
/// stranger's prompt in your box. Only reject when both sides actually
/// name a socket: the field is backfilled best-effort by the reconciler
/// and is routinely `None`, where dropping the row would be worse than the
/// collision it guards against.
fn on_this_server(agent: &Agent, here: Option<&str>) -> bool {
    match (agent.tmux_socket.as_deref(), here) {
        (Some(theirs), Some(ours)) => theirs == ours,
        _ => true,
    }
}

/// Sort key for "which agent speaks for this pane" — lower wins. Blocked
/// agents outrank busy ones because they're the reason you opened the
/// overlay; stopped rows sink below everything.
fn interest_rank(a: &Agent) -> u8 {
    match a.state {
        AgentState::WaitingChoice | AgentState::WaitingInput => 0,
        AgentState::Error => 1,
        AgentState::Working => 2,
        AgentState::Starting => 3,
        AgentState::Idle => 4,
        AgentState::Stopped => 5,
    }
}

/// Colour for a state glyph and the box border that carries it.
///
/// Peek deliberately doesn't take a `[watch] theme`: it's a momentary
/// overlay answering "which pane needs me", so it stays on the same fixed
/// semantic palette as the status line rather than inheriting a theme.
/// (`crate::state_style` is the `owo_colors` one used by the table
/// printers — this is its ratatui counterpart.)
fn state_style(state: AgentState) -> Style {
    let color = match state {
        AgentState::Working => Color::Green,
        AgentState::WaitingInput => Color::Yellow,
        AgentState::WaitingChoice => Color::LightYellow,
        AgentState::Error => Color::Red,
        AgentState::Starting => Color::Cyan,
        AgentState::Idle => Color::Gray,
        AgentState::Stopped => Color::DarkGray,
    };
    Style::default().fg(color)
}

/// Where the overlay's own chrome goes relative to the client.
///
/// Both fields come from the same fact — which end of the client tmux's
/// status line occupies. Panes start below it, and the hint bar goes back
/// on top of it, so the overlay never has to steal a row from a pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Client row where window row 0 lands.
    pub origin_y: u16,
    /// Draw the hint bar on the first client row rather than the last.
    pub hint_at_top: bool,
}

impl From<Option<WindowFrame>> for Placement {
    fn from(frame: Option<WindowFrame>) -> Self {
        // No frame reading means no known status line; the popup then owns
        // the whole client, and the bottom row is the conventional home for
        // a hint bar.
        frame.map_or(Placement::default(), |f| Placement {
            origin_y: f.pane_origin_y(),
            hint_at_top: f.status_top,
        })
    }
}

fn draw(f: &mut Frame, cells: &[PeekCell], placement: Placement, zoomed: bool, pending: &str) {
    let area = f.area();
    f.render_widget(Clear, area);
    for cell in cells {
        // A zoomed window still reports the *unzoomed* rectangles for the
        // covered panes; drawing those would stamp boxes over screen the
        // zoomed pane now owns.
        if zoomed && !cell.geo.active {
            continue;
        }
        if let Some(rect) = cell_rect(&cell.geo, placement.origin_y, area) {
            render_cell(f, cell, rect);
        }
    }
    if area.height > 0 {
        // Land on the row tmux's status line occupies — the one row of the
        // client that never belongs to a pane. Guessing wrong here paints
        // the hint over a pane's content.
        let y = if placement.hint_at_top {
            area.y
        } else {
            area.y + area.height - 1
        };
        let hint = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(hint_line(cells, pending)), hint);
    }
}

/// Translate one pane's window-relative rectangle into client-relative
/// screen space, clipped to what the popup actually owns.
///
/// Returns `None` when the pane falls entirely outside the popup — which
/// happens legitimately when the layout changed between the geometry read
/// and the draw, so it's a skip rather than an error.
pub(crate) fn cell_rect(geo: &PaneGeometry, origin_y: u16, area: Rect) -> Option<Rect> {
    let x = area.x.checked_add(geo.left)?;
    let y = area.y.checked_add(geo.top)?.checked_add(origin_y)?;
    if x >= area.right() || y >= area.bottom() {
        return None;
    }
    let width = geo.width.min(area.right() - x);
    let height = geo.height.min(area.bottom() - y);
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

fn render_cell(f: &mut Frame, cell: &PeekCell, rect: Rect) {
    // The pane's own screen goes down first, dimmed, so the overlay reads
    // as something laid *over* your terminal rather than instead of it.
    // tmux popups have no transparency, so this redraw is the only way to
    // keep the context that tells you which pane you're looking at.
    render_backdrop(f, cell, rect);

    let header = header_spans(cell);
    let one_line = Rect { height: 1, ..rect };
    if rect.height < MIN_BORDERED_HEIGHT || rect.width < 4 {
        // Too small to frame — spend every cell on the label itself.
        f.render_widget(Clear, one_line);
        f.render_widget(Paragraph::new(Line::from(header)), one_line);
        return;
    }
    if cell.agent.is_none() {
        // No agent, nothing to narrate. A bare badge keeps the pane
        // jumpable without framing an empty box over its output.
        f.render_widget(Clear, one_line);
        f.render_widget(Paragraph::new(Line::from(header)), one_line);
        return;
    }
    // The box covers only what it needs, so the pane's content stays
    // readable below it.
    let rect = Rect {
        height: box_height(cell, rect),
        ..rect
    };
    // Wipe the backdrop out from under the box: the dim capture showing
    // through the box's own interior reads as corruption, not as depth.
    f.render_widget(Clear, rect);

    let accent = cell.agent.as_ref().map_or_else(
        || Style::default().fg(Color::DarkGray),
        |a| state_style(a.state),
    );
    let block = Block::bordered()
        .border_type(if cell.geo.active {
            BorderType::Double
        } else {
            BorderType::Rounded
        })
        .border_style(if cell.geo.active {
            accent.add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(Line::from(header.clone()));
    // Right-aligned on the same border row as the header. A box too
    // narrow to hold both would render them overlapping, so the stamp
    // yields to the identity that makes the pane jumpable.
    let block = match age_stamp(cell) {
        Some(stamp) if fits_alongside_header(&header, &stamp, rect.width) => block.title_top(
            Line::from(Span::styled(
                stamp,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ))
            .right_aligned(),
        ),
        _ => block,
    };
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Reserve the bottom row for the meta strip once there's a row to
    // spare above it; below that, body text is the better use of space.
    let (body, meta) = if inner.height >= 3 {
        (
            Rect {
                height: inner.height - 1,
                ..inner
            },
            Some(Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
            }),
        )
    } else {
        (inner, None)
    };

    f.render_widget(
        Paragraph::new(body_text(cell, body.width, body.height)),
        body,
    );
    if let Some(meta) = meta {
        if let Some(line) = meta_line(cell, meta.width) {
            f.render_widget(Paragraph::new(line), meta);
        }
    }
}

/// Paint the pane's captured screen as a dim backdrop.
fn render_backdrop(f: &mut Frame, cell: &PeekCell, rect: Rect) {
    if cell.capture.is_empty() {
        return;
    }
    let lines: Vec<Line> = cell
        .capture
        .iter()
        .take(rect.height as usize)
        .map(|raw| {
            Line::from(Span::styled(
                raw.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), rect);
}

/// How many rows the info box claims out of a pane's rectangle.
///
/// Only as many as the content needs, and never more than two thirds of
/// the pane — a box that swallowed the whole pane would put us back to an
/// opaque overlay, which is the thing the backdrop exists to avoid.
pub(crate) fn box_height(cell: &PeekCell, rect: Rect) -> u16 {
    let ceiling = (rect.height.saturating_mul(2) / 3).max(MIN_BORDERED_HEIGHT);
    let ceiling = ceiling.min(rect.height);
    let inner_width = rect.width.saturating_sub(2);
    // Ask for the content the box *would* render at full allowance, then
    // shrink to it. `body_text` is already budget-aware, so passing the
    // ceiling keeps the two in agreement about what fits.
    let content = body_text(cell, inner_width, ceiling.saturating_sub(2));
    // `body_text` is bounded by the row budget we just handed it, so the
    // count always fits — clamp rather than cast so a future change to
    // that contract can't silently wrap.
    let mut needed = u16::try_from(content.lines.len()).unwrap_or(u16::MAX);
    if meta_line(cell, inner_width).is_some() {
        needed += 1;
    }
    (needed + 2).clamp(MIN_BORDERED_HEIGHT, ceiling)
}

/// Whether the header and the age stamp can share the top border without
/// colliding. Both are drawn on the same row from opposite ends, and
/// ratatui will happily overlap them; the two corners plus a gap between
/// is the floor.
fn fits_alongside_header(header: &[Span<'static>], stamp: &str, width: u16) -> bool {
    let header_width: usize = header.iter().map(|s| display_width(&s.content)).sum();
    header_width + display_width(stamp) + 3 <= width as usize
}

/// `1 ● claude +2` — the box title, and the whole box when it's one row.
fn header_spans(cell: &PeekCell) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(
            format!(" {} ", cell.geo.pane_index),
            Style::default()
                .fg(Color::Black)
                .bg(if cell.geo.active {
                    Color::Cyan
                } else {
                    Color::Gray
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    if let Some(a) = &cell.agent {
        spans.push(Span::styled(
            crate::state_icon(a.state),
            state_style(a.state),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            agent_kind_short(a.kind),
            Style::default().fg(Color::White),
        ));
    } else {
        // No agent: name the process so the box still identifies the pane
        // rather than reading as an empty slot.
        let label = if cell.geo.command.is_empty() {
            "-".to_string()
        } else {
            cell.geo.command.clone()
        };
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    if cell.extra > 0 {
        spans.push(Span::styled(
            format!(" +{}", cell.extra),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw(" "));
    spans
}

/// Body text, allocated by priority into whatever rows the pane has.
///
/// Every tier degrades rather than truncating the tier below it: a 4-row
/// pane shows summary + prompt, a 10-row pane shows summary + prompt +
/// response. The order is deliberate — the summary answers "what is this
/// agent doing", which is the question the overlay exists for; the prompt
/// and response are the supporting evidence.
pub(crate) fn body_text(cell: &PeekCell, width: u16, height: u16) -> Text<'static> {
    let width = width as usize;
    let mut budget = height as usize;
    if width == 0 || budget == 0 {
        return Text::default();
    }
    let Some(agent) = cell.agent.as_ref() else {
        return Text::default();
    };
    let mut lines: Vec<Line> = Vec::new();

    if let Some(summary) = summary_source(agent) {
        // A tall pane can afford a second line of summary; a short one
        // must leave room for the prompt.
        let allowance = if budget >= 6 { 2 } else { 1 };
        push_tier(
            &mut lines,
            &mut budget,
            summary,
            "",
            allowance,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            width,
        );
    }
    if let Some(prompt) = agent.last_prompt.as_deref() {
        push_tier(
            &mut lines,
            &mut budget,
            prompt,
            glyph_prompt(),
            2,
            Style::default().fg(Color::White),
            width,
        );
    }
    if let Some(response) = agent.last_response.as_deref() {
        push_tier(
            &mut lines,
            &mut budget,
            response,
            glyph_response(),
            3,
            Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
            width,
        );
    }
    Text::from(lines)
}

/// Wrap one tier into at most `max_lines` rows (further capped by what's
/// left of the box), prefixing the first row with `glyph`.
fn push_tier(
    lines: &mut Vec<Line<'static>>,
    budget: &mut usize,
    raw: &str,
    glyph: &str,
    max_lines: usize,
    style: Style,
    width: usize,
) {
    let allowed = max_lines.min(*budget);
    if allowed == 0 {
        return;
    }
    let indent = display_width(glyph);
    let body_width = width.saturating_sub(indent);
    if body_width == 0 {
        return;
    }
    for (i, chunk) in wrap_clamped(raw, body_width, allowed)
        .into_iter()
        .enumerate()
    {
        let prefix = if i == 0 {
            glyph.to_string()
        } else {
            " ".repeat(indent)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(chunk, style),
        ]));
        *budget -= 1;
    }
}

/// ` 5m ago ` for the box's top-right corner — how long since the human
/// last prompted this agent.
///
/// It lives on the border rather than inline with the prompt text, where
/// it broke the reading flow of the very line it was annotating. The
/// border is chrome; a timestamp is chrome.
pub(crate) fn age_stamp(cell: &PeekCell) -> Option<String> {
    let at = cell.last_prompt_at?;
    Some(format!(
        " {} ",
        crate::relative_time(OffsetDateTime::now_utc(), at)
    ))
}

fn glyph_prompt() -> &'static str {
    match crate::icon_set() {
        IconSet::Unicode => "▸ ",
        IconSet::Ascii => "> ",
    }
}

fn glyph_response() -> &'static str {
    match crate::icon_set() {
        IconSet::Unicode => "◂ ",
        IconSet::Ascii => "< ",
    }
}

/// Summary source, degrading the same way `muxa watch` does: recap →
/// session title → nothing. `last_prompt` is deliberately *not* in this
/// chain — unlike watch's single summary column, peek renders the prompt
/// on its own line, and falling back to it here would print it twice.
fn summary_source(a: &Agent) -> Option<&str> {
    a.recap.as_deref().or(a.ai_title.as_deref())
}

/// `opus · ctx 62% · 5h 41%` — the bottom strip, dropped entirely when
/// none of its parts are known.
fn meta_line(cell: &PeekCell, width: u16) -> Option<Line<'static>> {
    let agent = cell.agent.as_ref()?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(model) = agent.model.as_deref() {
        parts.push(model.to_ascii_lowercase());
    }
    if let Some(ctx) = agent.context_used_pct {
        parts.push(format!("ctx {ctx:.0}%"));
    }
    if let Some(five) = agent.rate_limit_5h_pct {
        parts.push(format!("5h {five:.0}%"));
    }
    if parts.is_empty() {
        return None;
    }
    let text = clip_to_width(&parts.join(" · "), width as usize);
    Some(Line::from(Span::styled(
        text,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )))
}

/// Footer strip drawn over the row tmux's status line occupies. Leads
/// with the attention count when anything is blocked, because that is the
/// one fact worth stealing focus for.
pub(crate) fn hint_line(cells: &[PeekCell], pending: &str) -> Line<'static> {
    let blocked = cells
        .iter()
        .filter(|c| {
            c.agent
                .as_ref()
                .is_some_and(|a| attend::needs_attention(a.state))
        })
        .count();
    let mut spans = Vec::new();
    if blocked > 0 {
        let verb = if blocked == 1 { "needs" } else { "need" };
        spans.push(Span::styled(
            format!(" {blocked} {verb} you "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    if pending.is_empty() {
        spans.push(Span::styled(
            " 0-9 jump · r refresh · q/Esc close",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    } else {
        // Digits only pend when the window has enough panes to make them
        // ambiguous. Showing them is the difference between "waiting for
        // your second digit" and "the overlay ignored my keypress".
        spans.push(Span::styled(
            format!(" {pending}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            "… more digits, or Enter to take it",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// Plain-text rendering used by `--plain`, one line per pane.
pub(crate) fn plain_lines(cells: &[PeekCell]) -> Vec<String> {
    cells
        .iter()
        .map(|cell| {
            let (glyph, label) = match &cell.agent {
                Some(a) => (crate::state_icon(a.state), agent_kind_short(a.kind)),
                None => ("·", "-"),
            };
            let summary = cell
                .agent
                .as_ref()
                .and_then(|a| summary_source(a).or(a.last_prompt.as_deref()))
                .map_or_else(|| "-".to_string(), collapse);
            let age = cell.last_prompt_at.map_or_else(
                || "-".to_string(),
                |at| crate::relative_time(OffsetDateTime::now_utc(), at),
            );
            format!(
                "{} {} {} {:<8} {:>8}  {}",
                cell.geo.pane_index, cell.geo.pane_id, glyph, label, age, summary
            )
        })
        .collect()
}

/// Greedy word wrap into at most `max_lines` rows of `width` display
/// columns, ellipsizing what doesn't fit.
///
/// Width is measured in terminal columns, not `char`s: prompts and recaps
/// here are routinely CJK, where a `chars().take(n)` budget overflows the
/// box by up to 2×. Words longer than the line (paths, URLs, hashes) are
/// broken mid-word rather than pushed out of view.
pub(crate) fn wrap_clamped(raw: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let text = collapse(raw);
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut overflowed = false;

    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let word_width = display_width(word);
        let sep = usize::from(!current.is_empty());
        if current_width + sep + word_width <= width {
            if sep == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_width += sep + word_width;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            if lines.len() == max_lines {
                overflowed = true;
                break;
            }
        }
        // A word wider than the whole line can never be placed by
        // wrapping; chop it across as many lines as we're allowed.
        if word_width > width {
            let mut rest = word;
            loop {
                let (head, tail) = split_at_width(rest, width);
                if head.is_empty() {
                    break;
                }
                if lines.len() == max_lines {
                    overflowed = true;
                    break;
                }
                if tail.is_empty() {
                    current = head.to_string();
                    current_width = display_width(head);
                    break;
                }
                lines.push(head.to_string());
                rest = tail;
            }
            if overflowed {
                break;
            }
        } else {
            current = word.to_string();
            current_width = word_width;
        }
    }
    if !overflowed && !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    } else if !current.is_empty() {
        overflowed = true;
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        overflowed = true;
    }
    if overflowed {
        if let Some(last) = lines.last_mut() {
            *last = ellipsize(last, width);
        }
    }
    lines
}

/// Collapse a multi-line, ragged value to a single spaced line. Prompts
/// and recaps carry newlines and indentation that would otherwise eat the
/// whole box.
fn collapse(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Split `s` at the last boundary whose prefix fits in `width` columns.
fn split_at_width(s: &str, width: usize) -> (&str, &str) {
    let mut used = 0usize;
    for (idx, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            return s.split_at(idx);
        }
        used += w;
    }
    (s, "")
}

/// Clip to `width` columns, replacing the tail with `…` when it doesn't fit.
fn clip_to_width(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    ellipsize(s, width)
}

/// Truncate to fit `width` columns *including* a trailing ellipsis.
fn ellipsize(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let (head, _) = split_at_width(s, width - 1);
    format!("{head}…")
}

/// Puts the terminal back on every exit path — including a panic, which
/// would otherwise strand a bare `muxa peek` in raw mode on the alternate
/// screen. (Inside a popup tmux tears the pty down for us; outside one,
/// nothing does.)
struct TerminalGuard {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
}

impl TerminalGuard {
    fn new(terminal: Terminal<CrosstermBackend<Stdout>>) -> Self {
        Self {
            terminal: Some(terminal),
        }
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        self.terminal.as_mut().expect("terminal present")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(mut t) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = execute!(t.backend_mut(), LeaveAlternateScreen);
            let _ = t.show_cursor();
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // The popup is its own screen already, but entering the alternate
    // screen keeps the restore path identical to the rest of the TUIs —
    // and matters for `muxa peek` run bare in a shell.
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::state::Agent;
    use muxa::AgentKind;
    use ratatui::backend::TestBackend;

    fn geo(
        index: &str,
        left: u16,
        top: u16,
        width: u16,
        height: u16,
        active: bool,
    ) -> PaneGeometry {
        PaneGeometry {
            pane_id: format!("%{index}"),
            pane_index: index.into(),
            left,
            top,
            width,
            height,
            active,
            command: "zsh".into(),
        }
    }

    fn agent(pane: &str, state: AgentState) -> Agent {
        let now = time::OffsetDateTime::now_utc();
        Agent {
            kind: AgentKind::ClaudeCode,
            session_id: format!("sess-{pane}"),
            surface: None,
            pane: Some(pane.into()),
            tmux_socket: None,
            tmux_session: None,
            cwd: None,
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            state,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
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

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map_or("", ratatui::buffer::Cell::symbol))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cell_rect_offsets_by_status_line() {
        let area = Rect::new(0, 0, 120, 40);
        // Status at the top pushes window row 0 down to client row 1.
        let rect = cell_rect(&geo("0", 0, 0, 120, 19, true), 1, area).unwrap();
        assert_eq!((rect.x, rect.y), (0, 1));
        assert_eq!((rect.width, rect.height), (120, 19));
    }

    #[test]
    fn cell_rect_clips_instead_of_overflowing() {
        let area = Rect::new(0, 0, 80, 24);
        // A pane whose rectangle runs past the popup (layout changed
        // under us) is clipped, not dropped and not drawn out of bounds.
        let rect = cell_rect(&geo("1", 40, 20, 60, 20, false), 0, area).unwrap();
        assert_eq!((rect.x, rect.y), (40, 20));
        assert_eq!((rect.width, rect.height), (40, 4));
        // Entirely off-screen is a skip.
        assert!(cell_rect(&geo("2", 200, 0, 20, 5, false), 0, area).is_none());
        assert!(cell_rect(&geo("3", 0, 30, 20, 5, false), 0, area).is_none());
    }

    #[test]
    fn zoom_draws_only_the_active_pane_but_still_counts_the_rest() {
        // tmux leaves stale rectangles on the covered siblings, so drawing
        // them would paint boxes over the zoomed pane's screen. The hidden
        // agent must still reach the footer count — a pane you cannot see
        // is exactly the one you need told about.
        let panes = vec![geo("0", 0, 0, 20, 5, false), geo("1", 0, 0, 20, 11, true)];
        let cells = build_cells(
            panes,
            &[
                agent("%0", AgentState::WaitingInput),
                agent("%1", AgentState::Working),
            ],
        );
        assert_eq!(cells.len(), 2, "both panes keep a cell");
        assert!(line_text(&hint_line(&cells, "")).starts_with(" 1 needs you "));

        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), true, ""))
            .unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains(" 1 "), "{rendered}");
        assert!(
            !rendered.contains(" 0 "),
            "the covered pane must not be painted over the zoomed one: {rendered}"
        );

        // Unzoomed, both are drawn — with the real (non-overlapping)
        // rectangles tmux reports once the zoom is released.
        let split = build_cells(
            vec![geo("0", 0, 0, 20, 5, false), geo("1", 0, 6, 20, 5, true)],
            &[],
        );
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal
            .draw(|f| draw(f, &split, Placement::default(), false, ""))
            .unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains(" 0 "), "{rendered}");
        assert!(rendered.contains(" 1 "), "{rendered}");
    }

    #[test]
    fn agent_from_another_tmux_server_is_not_borrowed() {
        // Pane ids are unique per server, so `%0` on socket `amux` must not
        // narrate `%0` here.
        let mut theirs = agent("%0", AgentState::Working);
        theirs.tmux_socket = Some("amux".into());
        theirs.ai_title = Some("someone else's work".into());
        assert!(!on_this_server(&theirs, Some("default")));
        assert!(on_this_server(&theirs, Some("amux")));

        // Unknown on either side means "can't tell" — keep the row rather
        // than blanking a box, since the field is backfilled best-effort
        // and is routinely absent.
        let unknown = agent("%0", AgentState::Working);
        assert!(on_this_server(&unknown, Some("default")));
        assert!(on_this_server(&theirs, None));
    }

    #[test]
    fn digits_resolve_and_wait_only_when_ambiguous() {
        let few = build_cells(
            vec![geo("0", 0, 0, 20, 5, true), geo("1", 0, 6, 20, 5, false)],
            &[],
        );
        // Nine-or-fewer panes: the first keypress is the jump.
        assert!(matches!(resolve_typed("1", &few), Selection::Hit(id) if id == "%1"));
        assert!(matches!(resolve_typed("7", &few), Selection::Miss));

        // With a pane 10, `1` is a prefix — jumping immediately would make
        // pane 10 unreachable.
        let many = build_cells(
            vec![
                geo("1", 0, 0, 20, 5, true),
                geo("10", 0, 6, 20, 5, false),
                geo("11", 0, 12, 20, 5, false),
            ],
            &[],
        );
        assert!(matches!(resolve_typed("1", &many), Selection::Prefix));
        assert!(matches!(resolve_typed("10", &many), Selection::Hit(id) if id == "%10"));
        assert!(matches!(resolve_typed("19", &many), Selection::Miss));

        // Pane 1 is reachable only via Enter: every continuation of "1"
        // names a different pane, so typing alone can never commit to it.
        assert_eq!(exact_match("1", &many).as_deref(), Some("%1"));
        assert_eq!(exact_match("19", &many), None);
        assert_eq!(exact_match("", &many), None);
    }

    #[test]
    fn pending_digits_are_shown_rather_than_swallowed() {
        // A digit that pends looks identical to a dropped keypress unless
        // the footer says otherwise.
        let cells = build_cells(
            vec![geo("1", 0, 0, 40, 5, true), geo("10", 0, 6, 40, 5, false)],
            &[],
        );
        let idle = line_text(&hint_line(&cells, ""));
        assert!(idle.contains("0-9 jump"), "{idle}");

        let waiting = line_text(&hint_line(&cells, "1"));
        assert!(waiting.contains(" 1"), "{waiting}");
        assert!(waiting.contains("Enter"), "{waiting}");
        assert!(!waiting.contains("0-9 jump"), "{waiting}");
    }

    #[test]
    fn blocked_agent_speaks_for_a_shared_pane() {
        // A restarted session leaves a stopped husk behind; the box must
        // report the agent actually waiting on the human.
        let agents = vec![
            agent("%0", AgentState::Stopped),
            agent("%0", AgentState::WaitingChoice),
            agent("%0", AgentState::Working),
        ];
        let cells = build_cells(vec![geo("0", 0, 0, 80, 24, true)], &agents);
        assert_eq!(cells.len(), 1);
        assert_eq!(
            cells[0].agent.as_ref().unwrap().state,
            AgentState::WaitingChoice
        );
        assert_eq!(cells[0].extra, 2, "the other two are badged, not dropped");
    }

    #[test]
    fn pane_without_agent_still_gets_a_cell() {
        let cells = build_cells(vec![geo("0", 0, 0, 80, 24, true)], &[]);
        assert_eq!(cells.len(), 1);
        assert!(cells[0].agent.is_none());
        assert_eq!(cells[0].extra, 0);
    }

    #[test]
    fn body_degrades_with_pane_height() {
        let mut a = agent("%0", AgentState::Working);
        a.ai_title = Some("auth refactor".into());
        a.last_prompt = Some("fix the token check".into());
        a.last_response = Some("added a JWT expiry guard".into());
        let cell = PeekCell {
            geo: geo("0", 0, 0, 40, 10, true),
            agent: Some(a),
            extra: 0,
            last_prompt_at: None,
            capture: Vec::new(),
        };

        // One row: summary only — the question the overlay exists to answer.
        let one = body_text(&cell, 38, 1);
        assert_eq!(one.lines.len(), 1);
        assert!(line_text(&one.lines[0]).contains("auth refactor"));

        // Two rows: summary + prompt.
        let two = body_text(&cell, 38, 2);
        assert_eq!(two.lines.len(), 2);
        assert!(line_text(&two.lines[1]).contains("fix the token check"));

        // Roomy: response earns its line too.
        let full = body_text(&cell, 38, 8);
        let joined: String = full
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("added a JWT expiry guard"), "{joined}");
    }

    #[test]
    fn body_never_exceeds_its_row_budget() {
        let mut a = agent("%0", AgentState::Working);
        a.recap = Some("word ".repeat(200));
        a.last_prompt = Some("word ".repeat(200));
        a.last_response = Some("word ".repeat(200));
        let cell = PeekCell {
            geo: geo("0", 0, 0, 40, 10, true),
            agent: Some(a),
            extra: 0,
            last_prompt_at: None,
            capture: Vec::new(),
        };
        for height in 0..12u16 {
            let text = body_text(&cell, 20, height);
            assert!(
                text.lines.len() <= height as usize,
                "height {height} produced {} lines",
                text.lines.len()
            );
        }
    }

    #[test]
    fn summary_does_not_repeat_the_prompt() {
        // watch's summary column falls back to last_prompt; peek must not,
        // or a agent with no recap prints the same string twice.
        let mut a = agent("%0", AgentState::Working);
        a.last_prompt = Some("only a prompt".into());
        let cell = PeekCell {
            geo: geo("0", 0, 0, 40, 10, true),
            agent: Some(a),
            extra: 0,
            last_prompt_at: None,
            capture: Vec::new(),
        };
        let text = body_text(&cell, 38, 6);
        let hits = text
            .lines
            .iter()
            .filter(|l| line_text(l).contains("only a prompt"))
            .count();
        assert_eq!(hits, 1);
    }

    #[test]
    fn wrap_measures_columns_not_chars() {
        // Korean is two columns per char: a char-count budget would print
        // 16 columns of text into an 8-column box.
        let lines = wrap_clamped("토큰검증을고쳐줘", 8, 2);
        for line in &lines {
            assert!(
                display_width(line) <= 8,
                "{line:?} is {} columns",
                display_width(line)
            );
        }
        assert!(!lines.is_empty());
    }

    #[test]
    fn wrap_breaks_words_longer_than_the_line() {
        let lines = wrap_clamped("crates/muxa-cli/src/peek.rs::render_cell", 10, 3);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(display_width(line) <= 10, "{line:?}");
        }
        assert!(lines.last().unwrap().ends_with('…'), "{lines:?}");
    }

    #[test]
    fn wrap_collapses_newlines_and_marks_truncation() {
        let lines = wrap_clamped("first line\n\n   second line   \nthird", 12, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with('…'));
        assert!(display_width(&lines[0]) <= 12);
    }

    #[test]
    fn wrap_that_fits_is_not_ellipsized() {
        let lines = wrap_clamped("short enough", 20, 2);
        assert_eq!(lines, vec!["short enough".to_string()]);
    }

    #[test]
    fn wrap_handles_degenerate_bounds() {
        assert!(wrap_clamped("anything", 0, 3).is_empty());
        assert!(wrap_clamped("anything", 10, 0).is_empty());
        assert!(wrap_clamped("   \n  ", 10, 3).is_empty());
    }

    #[test]
    fn meta_strip_drops_when_nothing_is_known() {
        let cell = PeekCell {
            geo: geo("0", 0, 0, 40, 10, true),
            agent: Some(agent("%0", AgentState::Working)),
            extra: 0,
            last_prompt_at: None,
            capture: Vec::new(),
        };
        assert!(meta_line(&cell, 30).is_none());

        let mut a = agent("%0", AgentState::Working);
        a.model = Some("Opus".into());
        a.context_used_pct = Some(62.4);
        a.rate_limit_5h_pct = Some(41.0);
        let cell = PeekCell {
            agent: Some(a),
            ..cell
        };
        let line = meta_line(&cell, 30).unwrap();
        assert_eq!(line_text(&line), "opus · ctx 62% · 5h 41%");
    }

    #[test]
    fn hint_leads_with_the_attention_count() {
        let quiet = build_cells(vec![geo("0", 0, 0, 80, 24, true)], &[]);
        assert!(!line_text(&hint_line(&quiet, "")).contains("need"));

        let cells = build_cells(
            vec![geo("0", 0, 0, 80, 12, true), geo("1", 0, 13, 80, 11, false)],
            &[
                agent("%0", AgentState::WaitingInput),
                agent("%1", AgentState::WaitingChoice),
            ],
        );
        assert!(line_text(&hint_line(&cells, "")).starts_with(" 2 need you "));
    }

    #[test]
    fn keys_map_to_actions() {
        assert!(matches!(
            classify(KeyEvent::from(KeyCode::Char('3'))),
            Action::Digit('3')
        ));
        assert!(matches!(
            classify(KeyEvent::from(KeyCode::Esc)),
            Action::Dismiss
        ));
        assert!(matches!(
            classify(KeyEvent::from(KeyCode::Char('q'))),
            Action::Dismiss
        ));
        // The key that opened the overlay should also close it.
        assert!(matches!(
            classify(KeyEvent::from(KeyCode::Char('Q'))),
            Action::Dismiss
        ));
        assert!(matches!(
            classify(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Dismiss
        ));
        assert!(matches!(
            classify(KeyEvent::from(KeyCode::Char('r'))),
            Action::Refresh
        ));
        assert!(matches!(
            classify(KeyEvent::from(KeyCode::Enter)),
            Action::Commit
        ));
        // Ctrl-digit is a terminal chord, not a jump request.
        assert!(matches!(
            classify(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL)),
            Action::Ignore
        ));
    }

    #[test]
    fn draws_every_pane_box_within_bounds() {
        let mut a = agent("%1", AgentState::WaitingInput);
        a.ai_title = Some("auth refactor".into());
        a.last_prompt = Some("fix the token check".into());
        let cells = build_cells(
            vec![geo("0", 0, 0, 40, 11, false), geo("1", 0, 12, 40, 11, true)],
            &[a],
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), false, ""))
            .unwrap();
        let rendered = screen(&terminal);
        // Pane digits are the jump affordance — both must be visible.
        assert!(rendered.contains(" 0 "), "{rendered}");
        assert!(rendered.contains(" 1 "), "{rendered}");
        assert!(rendered.contains("auth refactor"), "{rendered}");
        assert!(rendered.contains("fix the token check"), "{rendered}");
        assert!(rendered.contains("0-9 jump"), "{rendered}");
    }

    #[test]
    fn tiny_pane_drops_the_border_for_the_label() {
        let cells = build_cells(
            vec![geo("7", 0, 0, 20, 2, true)],
            &[agent("%7", AgentState::Working)],
        );
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), false, ""))
            .unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains(" 7 "), "{rendered}");
        assert!(
            !rendered.contains('╔') && !rendered.contains('╭'),
            "a 2-row pane has no room to spend on a frame: {rendered}"
        );
    }

    #[test]
    fn hint_bar_lands_on_the_status_row() {
        // The status line is the only client row that belongs to no pane.
        // With it at the top, a bottom-anchored hint would overwrite the
        // last pane's content instead.
        let frame = WindowFrame {
            window_width: 20,
            window_height: 5,
            client_width: 20,
            client_height: 6,
            status_top: true,
        };
        let placement = Placement::from(Some(frame));
        assert_eq!(placement.origin_y, 1);
        assert!(placement.hint_at_top);

        let cells = build_cells(vec![geo("0", 0, 0, 20, 5, true)], &[]);
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, placement, false, ""))
            .unwrap();
        let rows: Vec<String> = screen(&terminal).lines().map(str::to_string).collect();
        assert!(rows[0].contains("0-9 jump"), "{rows:#?}");
        // The pane's box is pushed down clear of the status row, and the
        // pane's own last row stays its own — nothing is sacrificed to the
        // hint at either end.
        assert!(rows[1].contains(" 0 "), "{rows:#?}");
        assert!(!rows[5].contains("0-9 jump"), "{rows:#?}");

        // Status at the bottom (tmux's default) puts the hint back on the
        // last row and starts panes at row 0.
        let bottom = Placement::from(Some(WindowFrame {
            status_top: false,
            ..frame
        }));
        assert_eq!(bottom.origin_y, 0);
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, bottom, false, ""))
            .unwrap();
        let rows: Vec<String> = screen(&terminal).lines().map(str::to_string).collect();
        assert!(rows[0].contains(" 0 "), "{rows:#?}");
        assert!(rows[5].contains("0-9 jump"), "{rows:#?}");
    }

    #[test]
    fn pane_content_stays_visible_under_the_box() {
        // The whole point of the backdrop: you still see what the pane was
        // showing, so the overlay reads as a layer over your terminal
        // rather than a replacement for it.
        let mut a = agent("%0", AgentState::Working);
        a.ai_title = Some("auth refactor".into());
        a.last_prompt = Some("fix the token check".into());
        let mut cells = build_cells(vec![geo("0", 0, 0, 30, 12, true)], &[a]);
        cells[0].capture = (0..12).map(|i| format!("output line {i}")).collect();

        // The client is one row taller than the window — that row is the
        // status line, and it's where the hint bar goes.
        let mut terminal = Terminal::new(TestBackend::new(30, 13)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), false, ""))
            .unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("auth refactor"), "{rendered}");

        let rows: Vec<String> = rendered.lines().map(str::to_string).collect();
        let box_rows = box_height(&cells[0], Rect::new(0, 0, 30, 12)) as usize;
        // Inside the box, the backdrop must be wiped: dim capture text
        // showing through the box's own interior reads as corruption, not
        // as depth. (Checked row-wise — "output line 1" is a substring of
        // "output line 11", which legitimately appears further down.)
        for (i, row) in rows.iter().take(box_rows).enumerate() {
            assert!(
                !row.contains("output line"),
                "row {i} inside the box leaked the backdrop: {rows:#?}"
            );
        }
        // Below it, the pane's own output survives to its last row.
        assert!(rows[box_rows].contains("output line"), "{rows:#?}");
        assert!(rendered.contains("output line 11"), "{rows:#?}");
    }

    #[test]
    fn box_never_swallows_the_whole_pane() {
        // A box that filled its pane would put us back to an opaque
        // overlay, which is exactly what the backdrop exists to avoid.
        let mut a = agent("%0", AgentState::Working);
        a.recap = Some("word ".repeat(200));
        a.last_prompt = Some("word ".repeat(200));
        a.last_response = Some("word ".repeat(200));
        a.model = Some("Opus".into());
        let cell = PeekCell {
            geo: geo("0", 0, 0, 40, 30, true),
            agent: Some(a),
            extra: 0,
            last_prompt_at: None,
            capture: Vec::new(),
        };
        for height in MIN_BORDERED_HEIGHT..30 {
            let rect = Rect::new(0, 0, 40, height);
            let h = box_height(&cell, rect);
            assert!(h >= MIN_BORDERED_HEIGHT, "height {height} gave {h}");
            assert!(h <= height, "height {height} gave {h}");
            // Two thirds is the ceiling, except where the minimum box is
            // already taller than that.
            let ceiling = (height * 2 / 3).max(MIN_BORDERED_HEIGHT);
            assert!(h <= ceiling, "height {height} gave {h}, ceiling {ceiling}");
        }
    }

    #[test]
    fn box_shrinks_to_the_content_it_has() {
        // A pane whose agent has nothing to say should not reserve rows it
        // will only render blank.
        let quiet = PeekCell {
            geo: geo("0", 0, 0, 40, 24, true),
            agent: Some(agent("%0", AgentState::Idle)),
            extra: 0,
            last_prompt_at: None,
            capture: Vec::new(),
        };
        let rect = Rect::new(0, 0, 40, 24);
        assert_eq!(box_height(&quiet, rect), MIN_BORDERED_HEIGHT);

        let mut a = agent("%0", AgentState::Working);
        a.ai_title = Some("auth refactor".into());
        a.last_prompt = Some("fix the token check".into());
        a.model = Some("Opus".into());
        let talkative = PeekCell {
            agent: Some(a),
            ..quiet
        };
        assert!(box_height(&talkative, rect) > MIN_BORDERED_HEIGHT);
    }

    #[test]
    fn age_stamp_sits_on_the_border_not_in_the_prompt() {
        let mut a = agent("%0", AgentState::Working);
        a.ai_title = Some("auth refactor".into());
        a.last_prompt = Some("fix the token check".into());
        let mut cells = build_cells(vec![geo("0", 0, 0, 40, 12, true)], &[a]);
        cells[0].last_prompt_at = Some(OffsetDateTime::now_utc() - time::Duration::minutes(5));

        let mut terminal = Terminal::new(TestBackend::new(40, 13)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), false, ""))
            .unwrap();
        let rows: Vec<String> = screen(&terminal).lines().map(str::to_string).collect();

        // On the border, hugging the right corner.
        assert!(rows[0].contains("5m ago"), "{rows:#?}");
        assert!(rows[0].trim_end().ends_with('╗'), "{rows:#?}");
        // Not wedged into the prompt line, which broke the reading flow of
        // the very text it annotated.
        let prompt_row = rows
            .iter()
            .find(|r| r.contains("fix the token check"))
            .expect("prompt line rendered");
        assert!(!prompt_row.contains("ago"), "{prompt_row}");
        // The glyph leads the text directly, with nothing wedged between.
        assert!(prompt_row.contains("▸ fix the token check"), "{prompt_row}");
    }

    #[test]
    fn unknown_prompt_time_leaves_the_border_bare() {
        // An invented "0s ago" would be a lie about when the human last
        // showed up — the question the stamp exists to answer.
        let mut a = agent("%0", AgentState::Working);
        a.last_prompt = Some("fix the token check".into());
        let cells = build_cells(vec![geo("0", 0, 0, 40, 12, true)], &[a]);
        assert!(cells[0].last_prompt_at.is_none());
        assert!(age_stamp(&cells[0]).is_none());

        let mut terminal = Terminal::new(TestBackend::new(40, 13)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), false, ""))
            .unwrap();
        assert!(!screen(&terminal).contains("ago"));
    }

    #[test]
    fn narrow_box_drops_the_stamp_rather_than_overlapping_the_header() {
        // ratatui draws both titles on the same row from opposite ends and
        // will happily let them collide; the digit is what makes the pane
        // jumpable, so the stamp is what yields.
        let mut a = agent("%0", AgentState::Working);
        a.last_prompt = Some("fix it".into());
        let mut cells = build_cells(vec![geo("0", 0, 0, 16, 8, true)], &[a]);
        cells[0].last_prompt_at = Some(OffsetDateTime::now_utc() - time::Duration::minutes(5));

        let header = header_spans(&cells[0]);
        let stamp = age_stamp(&cells[0]).unwrap();
        assert!(!fits_alongside_header(&header, &stamp, 16));
        assert!(fits_alongside_header(&header, &stamp, 40));

        let mut terminal = Terminal::new(TestBackend::new(16, 9)).unwrap();
        terminal
            .draw(|f| draw(f, &cells, Placement::default(), false, ""))
            .unwrap();
        let rows: Vec<String> = screen(&terminal).lines().map(str::to_string).collect();
        assert!(!rows[0].contains("ago"), "{rows:#?}");
        assert!(rows[0].contains(" 0 "), "the digit survives: {rows:#?}");
    }

    #[test]
    fn backdrops_survive_an_agent_refresh() {
        // Agent state re-reads every second; backdrops must ride along
        // rather than be re-shelled, or the "capture once" saving is lost
        // the first time a state flips.
        let mut before = build_cells(
            vec![geo("0", 0, 0, 20, 5, true), geo("1", 0, 6, 20, 5, false)],
            &[],
        );
        before[0].capture = vec!["pane zero output".into()];
        before[1].capture = vec!["pane one output".into()];

        let mut after = build_cells(
            vec![
                geo("0", 0, 0, 20, 5, true),
                geo("1", 0, 6, 20, 5, false),
                // A pane split into existence since the last capture.
                geo("2", 0, 12, 20, 5, false),
            ],
            &[],
        );
        carry_captures(&before, &mut after);
        assert_eq!(after[0].capture, vec!["pane zero output".to_string()]);
        assert_eq!(after[1].capture, vec!["pane one output".to_string()]);
        // No backdrop yet for the newcomer — it gets one on the next `r`,
        // which beats inventing one from a pane we never read.
        assert!(after[2].capture.is_empty());
    }

    #[test]
    fn plain_mode_lists_one_line_per_pane() {
        let mut a = agent("%1", AgentState::Working);
        a.ai_title = Some("auth refactor".into());
        let cells = build_cells(
            vec![geo("0", 0, 0, 40, 11, false), geo("1", 0, 12, 40, 11, true)],
            &[a],
        );
        let lines = plain_lines(&cells);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("0 %0 "));
        assert!(lines[1].contains("auth refactor"));
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}
