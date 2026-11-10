use super::{
    board_screen, board_window, collab_screen, detail_for, watch_theme, Alignment, App, Block,
    Borders, Cell, CollabLayout, CollabRow, Constraint, Direction, Frame, HighlightSpacing, Layout,
    Line, OffsetDateTime, Paragraph, Rect, Row, SequenceRow, Span, Table, TableState,
    WatchThemeSpec, Wrap, COLLAB_DETAIL_HEIGHT,
};

/// Below this the table is too short to be worth splitting, so the body stays
/// one `M` away instead of leaving three rows of listing.
const COLLAB_DETAIL_MIN_HEIGHT: u16 = 16;

/// The board's frame: counts on the left, the all-sessions activity in the
/// corner when it fits.
fn board_block(
    app: &App,
    area: Rect,
    works: usize,
    waiting: usize,
    theme: WatchThemeSpec,
) -> Block<'static> {
    use std::fmt::Write as _;
    let mut title = format!(" Board · {works} Works · {waiting} need you ");
    // Totals describe the rows on screen; executions no Work claims are said
    // apart so the board never reads as everything that is running.
    if app.board.unlinked() > 0 {
        let _ = write!(title, "· {} unlinked ", app.board.unlinked());
    }
    let _ = write!(
        title,
        "· by {} ",
        board_screen::sort_label(app.board.sort())
    );
    // Activity is for every session in the window, not the Works on screen,
    // so it sits apart from the counts and never follows the filter. When the
    // two titles would collide it shortens, then gives way to the counts.
    let list_width = if area.width >= 140 {
        area.width * 60 / 100
    } else {
        area.width
    };
    let activity = fit_corner_title(
        &title,
        &[
            format!(" {} ", app.board_stats.label(&board_window(app))),
            format!(" {} ", app.board_stats.figures()),
        ],
        list_width,
    );
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(title);
    if let Some(activity) = activity {
        block = block.title(Line::from(activity).right_aligned());
    }

    block
}

pub(super) fn render_board_screen(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let filter = app.search_query.clone();
    let visible = app.board.visible(&filter);
    let waiting = visible
        .iter()
        .filter(|work| board_screen::needs_operator(work))
        .count();
    let block = board_block(app, area, visible.len(), waiting, theme);

    if visible.is_empty() {
        let hint = if !app.board.loaded {
            "loading Works…"
        } else if app.board.is_empty() {
            "no Work yet — `tend work start` or `tend work up` creates one"
        } else {
            "no Work matches this filter"
        };
        // A scan that failed must not read as an empty board.
        let mut lines = vec![Line::from(hint)];
        lines.extend(app.board.notes.iter().map(|note| Line::from(note.clone())));
        f.render_widget(
            Paragraph::new(lines)
                .style(theme.dim_style())
                .block(block)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let now = OffsetDateTime::now_utc();
    let selected = app.board.selected_index(&filter);
    let detail = board_detail(app, &filter, now, theme);
    let notes = app.board.notes.clone();
    // The detail sits below the list on narrow terminals and beside it on
    // wide ones, sized to what it has to say.
    let detail_height = u16::try_from(detail.len() + notes.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height / 2);
    let (list_area, detail_area) = if area.width >= 140 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);
        (columns[0], Some(columns[1]))
    } else if area.height >= 14 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(detail_height.max(4))])
            .split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };
    let rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(index, work)| {
            board_screen::row(
                work,
                now,
                theme,
                index == selected,
                &app.board.active_of(work),
            )
        })
        .collect();
    let header = Row::new(
        board_screen::HEADERS
            .iter()
            .map(|header| Cell::from(*header).style(theme.table_header_style())),
    )
    .height(1);
    let table = Table::new(rows, board_screen::COLUMNS)
        .header(header)
        .block(block)
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::default();
    state.select(Some(selected));
    f.render_stateful_widget(table, list_area, &mut state);

    if let Some(detail_area) = detail_area {
        render_board_detail(f, detail_area, detail, notes, theme);
    }
}

/// The first corner title that fits beside `left` in a block `width` wide
/// (two border cells and a separating gap), or none.
pub(super) fn fit_corner_title(left: &str, candidates: &[String], width: u16) -> Option<String> {
    use unicode_width::UnicodeWidthStr;
    let room = usize::from(width).saturating_sub(left.width() + 3);
    candidates
        .iter()
        .find(|candidate| candidate.width() <= room)
        .cloned()
}

/// The selected Work's detail lines, with its collaboration count under
/// the title.
fn board_detail(
    app: &App,
    filter: &str,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    let Some(work) = app.board.selected_work(filter) else {
        return Vec::new();
    };
    let agents = board_screen::live_agents(work);
    let selected = agents.get(app.board.selected_agent_index(filter)).copied();
    let mut lines = board_screen::detail_lines(work, now, theme, selected);
    let at = 1.min(lines.len());
    let window = board_window(app);
    // Time in the panes the Work holds now: a pane it gave up takes its
    // history with it, and a pane it gained brings its own.
    let active = match app.board.active_of(work) {
        board_screen::WorkActivity::Loading => format!("active  … ({window})"),
        board_screen::WorkActivity::Unavailable => format!("active  unavailable ({window})"),
        board_screen::WorkActivity::Ambiguous(pane) => {
            format!("active  ? — {pane} is on more than one server, so its time is not split")
        }
        board_screen::WorkActivity::Known(active) => format!(
            "active  ACT {} · WACT {} in its current panes ({window})",
            board_screen::active_label(active.active_secs),
            board_screen::active_label(active.work_active_secs)
        ),
    };
    lines.insert(at, Line::from(Span::styled(active, theme.dim_style())));
    lines.insert(
        at,
        Line::from(Span::styled(
            app.board_workers.line(work),
            theme.dim_style(),
        )),
    );
    lines.insert(
        at,
        Line::from(Span::styled(app.board_collab.line(work), theme.dim_style())),
    );
    lines
}

fn render_board_detail(
    f: &mut Frame,
    area: Rect,
    mut lines: Vec<Line<'static>>,
    notes: Vec<String>,
    theme: WatchThemeSpec,
) {
    lines.extend(
        notes
            .into_iter()
            .map(|note| Line::from(Span::styled(note, theme.dim_style()))),
    );
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.border_style())
                    .border_type(theme.border_type)
                    .title(" Work "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[allow(clippy::too_many_lines)] // table and sequence share selection and detail-pane state
pub(super) fn render_collab_screen(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let filter = app.search_query.clone();
    let entries = app.collab.rows(app.watch_cfg.view, &filter);
    let visible = app.collab.visible(&filter).len();
    let selected = app.collab.selected_index();
    let title = format!(
        " Collab · fleet ({visible}) · {} ",
        collab_screen::layout_label(app.collab.layout())
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(title);

    if let Some(reason) = app.collab.unavailable.clone() {
        f.render_widget(
            Paragraph::new(reason)
                .style(theme.dim_style())
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    if entries.is_empty() {
        // "Loading" and "the fleet said nothing" look identical on an empty
        // grid, and one of them is a reason to keep waiting.
        let hint = if !app.collab.loaded {
            "loading the fleet's collaboration…"
        } else if app.collab.is_empty() {
            "no collaboration requests in any room yet — `m` sends the first one"
        } else {
            "no request matches this filter"
        };
        f.render_widget(
            Paragraph::new(hint)
                .style(theme.dim_style())
                .block(block)
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    // A row carries an excerpt, and the body is what the operator came for.
    // Below the table rather than beside it: a vertical split takes its width
    // out of the columns that identify both ends of a request.
    let (area, detail_area) = if area.height >= COLLAB_DETAIL_MIN_HEIGHT {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(COLLAB_DETAIL_HEIGHT)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let now = OffsetDateTime::now_utc();
    if app.collab.layout() == CollabLayout::Sequence {
        let sequence = app.collab.sequence_rows(&filter);
        let selected_id = app
            .collab
            .selected_request(&filter)
            .map(|request| request.id.as_str());
        let mut selected_row = None;
        let rows = sequence
            .iter()
            .enumerate()
            .map(|(row_index, entry)| {
                let request = match entry {
                    SequenceRow::Group { .. } => None,
                    SequenceRow::Request(request) | SequenceRow::Reply(request) => Some(*request),
                };
                let selected = request.is_some_and(|request| {
                    let hit = selected_id == Some(request.id.as_str());
                    if hit && matches!(entry, SequenceRow::Request(_)) {
                        selected_row = Some(row_index);
                    }
                    hit
                });
                collab_screen::sequence_row(entry, now, theme, selected)
            })
            .collect::<Vec<_>>();
        let header = Row::new(
            collab_screen::SEQUENCE_HEADERS
                .iter()
                .map(|header| Cell::from(*header).style(theme.table_header_style())),
        );
        let table = Table::new(rows, collab_screen::SEQUENCE_COLUMNS)
            .header(header)
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always);
        let mut state = TableState::default();
        state.select(selected_row);
        f.render_stateful_widget(table, area, &mut state);
        if let Some(detail_area) = detail_area {
            render_collab_detail(f, detail_area, app, theme);
        }
        return;
    }
    // The cursor counts requests, not rows: group headers are not selectable,
    // so the nth request is the nth *selectable* line.
    let mut request_index = 0usize;
    let mut selected_row = None;
    let rows: Vec<Row> = entries
        .iter()
        .enumerate()
        .map(|(row_index, entry)| {
            let is_selected = match entry {
                CollabRow::Group(_) => false,
                CollabRow::Request(_) => {
                    let hit = request_index == selected;
                    if hit {
                        selected_row = Some(row_index);
                    }
                    request_index += 1;
                    hit
                }
            };
            collab_screen::row(entry, now, theme, is_selected)
        })
        .collect();
    let header = Row::new(
        collab_screen::HEADERS
            .iter()
            .map(|header| Cell::from(*header).style(theme.table_header_style())),
    )
    .height(1);
    let table = Table::new(rows, collab_screen::COLUMNS)
        .header(header)
        .block(block)
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::default();
    state.select(selected_row);
    f.render_stateful_widget(table, area, &mut state);
    if let Some(detail_area) = detail_area {
        render_collab_detail(f, detail_area, app, theme);
    }
}

/// The selected request in full: both ends with their rooms, the contract it
/// was sent under, the body, and the reply when one has come back.
fn render_collab_detail(f: &mut Frame, area: Rect, app: &App, theme: WatchThemeSpec) {
    let filter = app.search_query.clone();
    let Some(request) = app.collab.selected_request(&filter) else {
        return;
    };
    let request = detail_for(app, request);
    let now = OffsetDateTime::now_utc();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(
                    "{} [{}]",
                    request.from.label(),
                    collab_screen::location(&request.from)
                ),
                theme.table_header_style(),
            ),
            Span::raw(" → "),
            Span::styled(
                format!(
                    "{} [{}]",
                    request.to.label(),
                    collab_screen::location(&request.to)
                ),
                theme.table_header_style(),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                "{:?} · {:?} · {:?} · {} ago · thread {}{}{}",
                request.kind,
                request.work_mode,
                request.status,
                collab_screen::age(now, request.created_at),
                request.thread_id.as_deref().unwrap_or(&request.id),
                request
                    .work_id
                    .as_deref()
                    .map_or_else(String::new, |work| format!(" · work {work}")),
                request
                    .run_id
                    .as_deref()
                    .map_or_else(String::new, |run| format!(" · run {run}")),
            ),
            theme.dim_style(),
        )),
        Line::raw(request.body.clone()),
    ];
    if let Some(reply) = &request.reply {
        lines.push(Line::from(Span::styled(
            format!("↳ {:?}: {}", reply.status, reply.body),
            theme.dim_style(),
        )));
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style())
                .border_type(theme.border_type)
                .title(format!(" {} ", request.id)),
        ),
        area,
    );
}
