use super::{
    bottom_prompt_rect, build_pane_capture_body, command_suggestions, help_overlay_text,
    icons_unicode, plural, relative_time, render_ask_composer, render_ask_overlay,
    render_board_screen, render_broadcast_report_overlay, render_collab_screen,
    render_collaboration_composer, render_collaboration_mailbox, render_footer,
    render_message_skill_editor, render_preview, render_rename_overlay, render_spawn, render_swarm,
    render_table, render_topology_inspector, render_tree_table, render_work_composer_overlay,
    render_work_table, rows_have_active_spinner, selected_tree_target_from, skew_banner,
    sort_label, state_age_label, state_summary_spans, tree_node_states, truncate_chars,
    watch_theme, wrap_input_rows, AgentState, App, Block, Borders, Clear, Color, Constraint,
    Direction, Frame, Layout, Line, Modifier, OffsetDateTime, Paragraph, PreviewMode, Rect,
    SessionNode, Span, SpawnComposer, Spinner, Style, TopologyNodeKey, TreeTarget, WatchEventKind,
    WatchLayout, WatchRow, WatchScreen, WatchThemeSpec, WatchView,
};

pub(crate) fn render(f: &mut Frame, app: &mut App) {
    // Advance the animation clock once per paint so the swarm view's dot
    // spinners cycle. Harmless for the table views (which ignore it).
    app.anim_frame = app.anim_frame.wrapping_add(1);
    app.sync_auto_expansion();
    // Sorting/filtering the canonical hierarchy is the most expensive pure
    // render calculation. Build it once and share it across header, table,
    // inspector, and footer instead of rebuilding it for every surface.
    let tree_targets = app.uses_tree().then(|| app.tree_targets());
    let tree_targets = tree_targets.as_deref();
    app.visible_spinner_active = if let Some(targets) = tree_targets {
        targets.iter().any(|target| {
            app.topology.find(&target.key).is_some_and(|node| {
                tree_node_states(node)
                    .into_iter()
                    .any(|state| matches!(state, AgentState::Working | AgentState::Starting))
            })
        })
    } else {
        rows_have_active_spinner(&app.rows)
    };
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], app, tree_targets);
    match app.preview.as_ref().map(|p| p.mode) {
        Some(PreviewMode::Fullscreen) => {
            render_preview(f, chunks[1], app);
        }
        Some(PreviewMode::Popup) => {
            // Render the table behind so the user keeps a sense of
            // "where am I in the list" — then `Clear` the popup area
            // (wipes the cells under it so the popup paints clean) and
            // render the preview on top.
            render_body(f, chunks[1], app, tree_targets);
            let popup_area = centered_rect(80, 70, chunks[1]);
            f.render_widget(Clear, popup_area);
            render_preview(f, popup_area, app);
        }
        None => {
            render_body(f, chunks[1], app, tree_targets);
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
    if app.collaboration_composer.is_some() {
        let skills_open = app
            .collaboration_composer
            .as_ref()
            .is_some_and(|composer| composer.skill_palette.is_some());
        let popup_area = message_composer_rect(chunks[1], skills_open);
        f.render_widget(Clear, popup_area);
        render_collaboration_composer(f, popup_area, app);
    }
    render_ask_overlay(f, chunks[1], app);
    render_broadcast_report_overlay(f, chunks[1], app);
    render_work_composer_overlay(f, chunks[1], app);
    render_rename_overlay(f, chunks[1], app);
    if let Some(menu) = app.agent_move.as_ref() {
        menu.render(f, centered_rect(85, 80, chunks[1]));
    }
    if app.ask_composer.is_some() {
        let skills_open = app
            .ask_composer
            .as_ref()
            .is_some_and(|ask| ask.skill_palette.is_some());
        let popup_area = message_composer_rect(chunks[1], skills_open);
        f.render_widget(Clear, popup_area);
        render_ask_composer(f, popup_area, app);
    }
    if let Some(spawn) = app.spawn.as_ref() {
        let popup_area = spawn_popup_rect(chunks[1], spawn);
        f.render_widget(Clear, popup_area);
        render_spawn(f, popup_area, app);
    }
    if app.command_palette.is_some() {
        let popup_area = command_popup_rect(chunks[1]);
        f.render_widget(Clear, popup_area);
        render_command_palette(f, popup_area, app);
    }
    if app.message_skill_editor.is_some() {
        let popup_area = message_skill_editor_rect(chunks[1]);
        f.render_widget(Clear, popup_area);
        render_message_skill_editor(f, popup_area, app);
    }
    if app.confirm.is_some() {
        // Confirmation is always the top-most overlay, including when it was
        // opened from ask history. 50 × 30 % keeps the popup small enough that
        // the context behind stays scannable while retaining the y/N hint.
        let popup_area = centered_rect(50, 30, chunks[1]);
        f.render_widget(Clear, popup_area);
        render_confirm(f, popup_area, app);
    }
    render_footer(f, chunks[2], app, tree_targets);
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
        // The exit keys live in the border, not in the matrix: the body
        // is sized to its line count and clipped by the terminal, so a
        // row added here could push "how to leave" off a short screen —
        // which is exactly what adding the ask bindings did.
        .title(Span::styled(
            " help · ?/F1/Esc closes · q/Ctrl-C quits watch ",
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

/// Bottom-anchored form: dir/name/agent rows plus however many rows the
/// prompt currently wraps to (capped at half the body so the table never
/// disappears under a long brief).
fn spawn_popup_rect(r: Rect, spawn: &SpawnComposer) -> Rect {
    let avail = usize::from(r.width.saturating_sub(2 + 9)).max(4);
    let prompt_rows = u16::try_from(wrap_input_rows(&spawn.prompt, avail).len())
        .unwrap_or(1)
        .max(1);
    let cap = (r.height / 2).max(6);
    let height = (5 + prompt_rows).min(cap).min(r.height);
    Rect {
        x: r.x,
        y: r.y + r.height - height,
        width: r.width,
        height,
    }
}

pub(super) fn message_composer_rect(r: Rect, skills_open: bool) -> Rect {
    if !skills_open {
        return bottom_prompt_rect(r);
    }
    let height = r.height.min(12);
    Rect {
        x: r.x,
        y: r.y.saturating_add(r.height.saturating_sub(height)),
        width: r.width,
        height,
    }
}

fn message_skill_editor_rect(r: Rect) -> Rect {
    let width = r.width.saturating_mul(80).saturating_div(100).max(52);
    centered_rect_by_size(width, r.height.min(7), r)
}

pub(super) struct VisiblePromptInput {
    pub(super) text: String,
    pub(super) skipped_chars: usize,
}

pub(super) fn truncate_prompt_input(input: &str, max_width: usize) -> VisiblePromptInput {
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
pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
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
/// The fleet-wide collaboration listing.
///
/// Deliberately its own table rather than the topology one: the columns
/// identify a request and both of its endpoints, and none of the node columns
/// (state, duration, prompt) mean anything for a message.
/// Rows the detail pane costs the table, plus its two borders.
pub(super) const COLLAB_DETAIL_HEIGHT: u16 = 9;
fn app_agent_states(app: &App) -> Vec<AgentState> {
    let mut states = Vec::new();
    for row in &app.rows {
        match row {
            WatchRow::Agent(agent) => states.push(agent.state),
            WatchRow::Work(session) => states.extend(session.agent_states.values().copied()),
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
pub(super) fn render_header(
    f: &mut Frame,
    area: Rect,
    app: &App,
    tree_targets: Option<&[TreeTarget]>,
) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let agents = app
        .rows
        .iter()
        .filter(|row| match row {
            WatchRow::Agent(_) => true,
            WatchRow::Work(work) => work.latest_agent.is_some(),
            WatchRow::BarePane(_) => false,
        })
        .count();
    let bare = app.rows.len().saturating_sub(agents);
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

    if !app.legacy_flat_table {
        spans.push(Span::styled(
            format!(
                "{} sessions · {} windows · {} panes",
                app.topology.sessions.len(),
                app.topology
                    .sessions
                    .iter()
                    .map(|session| session.windows.len())
                    .sum::<usize>(),
                app.topology
                    .sessions
                    .iter()
                    .map(SessionNode::pane_count)
                    .sum::<usize>()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    } else if app.watch_cfg.view == WatchView::Window {
        spans.push(Span::styled(
            format!("{} work{}", app.rows.len(), plural(app.rows.len())),
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

    // The skew bar outranks the raw daemon error. When the daemon is stale,
    // the error text is a symptom the reader cannot act on ("protocol
    // mismatch: server=4 client=6" names no remedy and no culprit), while the
    // bar names both. When there is no error at all, the bar is the only thing
    // that will ever mention the skew.
    let status_line = if let Some(banner) = skew_banner(app) {
        Line::from(Span::styled(
            banner,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    } else if let Some(e) = app.last_error.as_ref() {
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
        let visible = if app.uses_tree() {
            tree_targets.map_or(0, <[TreeTarget]>::len)
        } else {
            app.visible_targets().len()
        };
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
            "j/k move  ·  / filter  ·  : commands  ·  ? help",
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

/// Dispatch the main body according to presentation; granularity remains a
/// separate [`WatchView`] concern.
fn render_body(f: &mut Frame, area: Rect, app: &mut App, tree_targets: Option<&[TreeTarget]>) {
    let split_inspector = app.inspector_enabled
        // The inspector describes the selected *pane*, and the collab screen's
        // cursor is on a request. Halving the table to show an unrelated pane
        // costs the two columns that identify both ends of a message.
        && !matches!(app.watch_cfg.screen, WatchScreen::Collab | WatchScreen::Board)
        && area.width >= 120
        && if app.uses_tree() {
            selected_tree_target_from(app, tree_targets.unwrap_or_default()).is_some()
        } else {
            app.selected_pane().is_some()
        }
        && app.preview.is_none()
        && !app.help_open
        && !app.event_inbox_open
        && !app.collaboration_mailbox.open
        && !app.ask_panel.open;
    // The composer is deliberately absent from that list. It is a two-line
    // overlay, exactly like the prompt popup, which never hid the inspector
    // — so `Enter` kept the peer's state on screen while `m` blanked it,
    // for no reason a user could infer. The inspector is *most* wanted
    // while composing: it is where you check what the peer is doing before
    // asking it for something. The mailbox and help stay in the list
    // because those are full-height panels that need the width.
    app.inspector_visible = split_inspector;
    app.window_capture_target = if split_inspector && app.uses_tree() {
        selected_tree_target_from(app, tree_targets.unwrap_or_default()).and_then(|target| {
            match &target.key {
                TopologyNodeKey::Window(key)
                    if crate::mux_control::supported(key.session.endpoint.host) =>
                {
                    Some(key.clone())
                }
                TopologyNodeKey::Session(_)
                | TopologyNodeKey::Window(_)
                | TopologyNodeKey::Pane(_) => None,
            }
        })
    } else {
        None
    };
    if split_inspector {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(app.inspector_split.list_pct()),
                Constraint::Percentage(100 - app.inspector_split.list_pct()),
            ])
            .split(area);
        render_primary_body(f, columns[0], app, tree_targets);
        render_inspector(f, columns[1], app, tree_targets);
        return;
    }
    render_primary_body(f, area, app, tree_targets);
}

fn render_primary_body(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    tree_targets: Option<&[TreeTarget]>,
) {
    app.table_page_rows = usize::from(area.height.saturating_sub(3).max(1));
    if matches!(app.watch_cfg.screen, WatchScreen::Collab) {
        render_collab_screen(f, area, app);
        return;
    }
    if app.watch_cfg.screen == WatchScreen::Board {
        render_board_screen(f, area, app);
        return;
    }
    if app.legacy_flat_table {
        render_table(f, area, app);
    } else if app.watch_cfg.layout == WatchLayout::Work {
        render_work_table(f, area, app, tree_targets.unwrap_or_default());
    } else if app.watch_cfg.layout == WatchLayout::Swarm {
        render_swarm(f, area, app, tree_targets.unwrap_or_default());
    } else {
        render_tree_table(f, area, app, tree_targets.unwrap_or_default());
    }
}

fn render_inspector(f: &mut Frame, area: Rect, app: &App, tree_targets: Option<&[TreeTarget]>) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    if app.uses_tree() {
        render_topology_inspector(f, area, app, tree_targets.unwrap_or_default(), theme);
        return;
    }
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
        let summary = muxa::prompt_headline(
            agent
                .last_notification
                .as_deref()
                .or(agent.last_prompt.as_deref())
                .unwrap_or("—"),
        )
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
