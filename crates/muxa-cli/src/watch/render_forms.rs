use super::{
    active_ask_conversation, centered_rect, matching_skills, message_composer_rect,
    reply_status_badge, request_kind_badge, session_base_name, truncate_chars,
    truncate_prompt_input, visible_ask_entries, watch_theme, work_mode_badge, wrap_detail_text,
    wrap_input_rows, Alignment, App, Block, Borders, Clear, CollaborationComposeTarget,
    CollaborationComposer, Color, ComposeSendMode, Constraint, Direction, Frame, HintLevel, Layout,
    Line, MailboxDetail, MessageSkillEditorField, MessageSkillPalette, Modifier, Padding,
    Paragraph, Rect, Span, SpawnComposer, SpawnField, Style, TopologyNodeKey, VisiblePromptInput,
    WatchThemeSpec, WorkMode,
};

/// Draw the `w` composer over `area`, or nothing when it is closed. Folded
/// into one call so `render` stays inside its line budget.
pub(super) fn render_rename_overlay(f: &mut Frame, area: Rect, app: &App) {
    if app.rename.is_none() {
        return;
    }
    let popup_area = message_composer_rect(area, false);
    f.render_widget(Clear, popup_area);
    render_rename(f, popup_area, app);
}

fn render_rename(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some(rename) = app.rename.as_ref() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(Span::styled(
            // The level is in the title because the same key opens the form on
            // three different things, and "which one is this about" has to be
            // answerable without looking back at the cursor behind the popup.
            format!(" rename {} ", rename.level.label()),
            theme.action_badge().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let view = truncate_prompt_input(&rename.input, usize::from(inner.width.saturating_sub(2)));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> "),
            Span::raw(view.text.clone()),
        ]))
        .block(block),
        area,
    );
    let before: String = view
        .text
        .chars()
        .take(rename.cursor.saturating_sub(view.skipped_chars))
        .collect();
    let x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(before.width()).unwrap_or(u16::MAX));
    if x < inner.x + inner.width {
        f.set_cursor_position((x, inner.y));
    }
}

pub(super) fn render_work_composer_overlay(f: &mut Frame, area: Rect, app: &App) {
    if app.work_composer.is_none() {
        return;
    }
    let popup_area = message_composer_rect(area, false);
    f.render_widget(Clear, popup_area);
    render_work_composer(f, popup_area, app);
}

fn render_work_composer(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some(work) = app.work_composer.as_ref() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(Span::styled(
            " work up ",
            theme.action_badge().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let view = truncate_prompt_input(&work.input, usize::from(inner.width.saturating_sub(2)));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> "),
            Span::raw(view.text.clone()),
        ]))
        .block(block),
        area,
    );
    let before: String = view
        .text
        .chars()
        .take(work.cursor.saturating_sub(view.skipped_chars))
        .collect();
    let x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(before.width()).unwrap_or(u16::MAX));
    if x < inner.x + inner.width {
        f.set_cursor_position((x, inner.y));
    }
}

pub(super) fn ask_composer_starts_new_conversation(app: &App) -> bool {
    app.ask_composer.as_ref().is_some_and(|ask| {
        ask.new_conversation
            || (app.ask_conversations_available && active_ask_conversation(app).is_none())
    })
}

pub(super) fn ask_composer_title(app: &App, theme: WatchThemeSpec) -> Line<'static> {
    let starts_new = ask_composer_starts_new_conversation(app);
    let mut spans = vec![Span::styled(
        format!(" ask · {} ", app.ask_agent),
        theme.table_header_style(),
    )];

    if starts_new {
        spans.push(Span::styled(
            " NEW ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" starts when sent ", theme.dim_style()));
    } else {
        spans.push(Span::styled(
            " CONTINUE ",
            theme.action_badge().add_modifier(Modifier::BOLD),
        ));
        let target = active_ask_conversation(app).map_or_else(
            || "current conversation".into(),
            |conversation| {
                let position = app
                    .ask_conversations
                    .iter()
                    .position(|candidate| candidate.id == conversation.id)
                    .map(|index| index + 1);
                position.map_or_else(
                    || truncate_chars(&conversation.title, 36),
                    |position| {
                        format!(
                            "{position}/{} · {}",
                            app.ask_conversations.len(),
                            truncate_chars(&conversation.title, 36)
                        )
                    },
                )
            },
        );
        spans.push(Span::styled(format!(" {target} "), theme.dim_style()));
    }

    Line::from(spans)
}

pub(super) fn render_ask_composer(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some(ask) = app.ask_composer.as_ref() else {
        return;
    };
    if let Some(palette) = ask.skill_palette.as_ref() {
        let target = format!("ask · {}", app.ask_agent);
        render_message_skill_palette(f, area, app, &target, palette, theme);
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(ask_composer_title(app, theme));
    let inner = block.inner(area);
    let view = truncate_prompt_input(&ask.input, usize::from(inner.width.saturating_sub(2)));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> "),
            Span::raw(view.text.clone()),
        ]))
        .block(block),
        area,
    );
    let before: String = view
        .text
        .chars()
        .take(ask.cursor.saturating_sub(view.skipped_chars))
        .collect();
    let x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(before.width()).unwrap_or(u16::MAX));
    if x < inner.x + inner.width {
        f.set_cursor_position((x, inner.y));
    }
}

fn ask_status_badge(entry: &muxa::ask::AskEntry, theme: WatchThemeSpec) -> Span<'static> {
    match entry.status {
        muxa::ask::AskStatus::Running => Span::styled(" … ", theme.key_badge()),
        muxa::ask::AskStatus::Answered => {
            Span::styled(" ✓ ", Style::default().fg(Color::Black).bg(Color::Green))
        }
        muxa::ask::AskStatus::Failed => {
            Span::styled(" ✗ ", Style::default().fg(Color::White).bg(Color::Red))
        }
    }
}

fn ask_panel_title(app: &App, theme: WatchThemeSpec) -> Line<'static> {
    let active = active_ask_conversation(app);
    let conversation_position = active.and_then(|selected| {
        app.ask_conversations
            .iter()
            .position(|conversation| conversation.id == selected.id)
            .map(|index| index + 1)
    });
    let conversation_label = active.map_or_else(
        || {
            if app.ask_conversations_available {
                "new conversation".into()
            } else {
                app.ask_panel.filter.label().into()
            }
        },
        |conversation| conversation.title.clone(),
    );
    let agent_label = active.map_or(app.ask_agent.as_str(), |conversation| {
        conversation.agent.as_str()
    });
    Line::from(vec![
        Span::styled(" ask ", theme.accent_badge()),
        Span::styled(format!(" {agent_label} "), theme.table_header_style()),
        Span::styled(
            format!(" {} ", truncate_chars(&conversation_label, 36)),
            theme.action_badge(),
        ),
        Span::styled(
            conversation_position.map_or_else(
                || {
                    if app.ask_conversations_available {
                        format!(" {} conversations · 0 turns ", app.ask_conversations.len())
                    } else {
                        format!(" {} turns ", visible_ask_entries(app).len())
                    }
                },
                |position| {
                    format!(
                        " {position}/{} · {} turns ",
                        app.ask_conversations.len(),
                        visible_ask_entries(app).len()
                    )
                },
            ),
            theme.table_header_style(),
        ),
    ])
}

fn render_ask_panel(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(ask_panel_title(app, theme));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let visible = visible_ask_entries(app);
    if visible.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "no turns in this conversation — press a to continue or n to start another",
                    theme.dim_style(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "(needs [ask] enabled = true in config.toml)",
                    theme.dim_style(),
                )),
            ]),
            inner,
        );
        return;
    }

    let detail_height = match app.ask_panel.detail {
        MailboxDetail::Compact => inner.height / 3,
        MailboxDetail::Half => inner.height / 2,
        MailboxDetail::Most => inner.height.saturating_sub(3),
    }
    .max(3);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
        .split(inner);

    // Newest last matches how the conversation reads; the selection
    // starts there after every ask.
    let rows = usize::from(chunks[0].height).max(1);
    let width = usize::from(chunks[0].width).saturating_sub(2);
    let start = app
        .ask_panel
        .selected
        .saturating_sub(rows.saturating_sub(1));
    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(i, entry)| {
            let marker = if i == app.ask_panel.selected {
                Span::styled("> ", theme.action_badge())
            } else {
                Span::raw("  ")
            };
            let head = entry.prompt.replace('\n', " ");
            Line::from(vec![
                marker,
                ask_status_badge(entry, theme),
                Span::raw(" "),
                Span::raw(truncate_chars(&head, width.saturating_sub(6))),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let detail_block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border_style())
        .title(Span::styled(" answer ", theme.dim_style()));
    let dw = usize::from(detail_block.inner(chunks[1]).width).max(1);
    let dh = usize::from(detail_block.inner(chunks[1]).height).max(1);
    let detail = visible.get(app.ask_panel.selected).map_or_else(
        || vec![Line::from(Span::styled("-", theme.dim_style()))],
        |entry| ask_detail_lines(entry, dw, dh, theme),
    );
    f.render_widget(Paragraph::new(detail).block(detail_block), chunks[1]);
}

/// The ask history and its reader share one popup slot: the reader is
/// opened from the list and returns to it, so only one is ever on screen.
pub(super) fn render_ask_overlay(f: &mut Frame, area: Rect, app: &mut App) {
    if !app.ask_panel.open {
        return;
    }
    // The reader takes more of the screen than the list it came from:
    // giving a long answer somewhere to be read is the entire point.
    let reading = app.ask_panel.reader.is_some();
    let popup_area = if reading {
        centered_rect(92, 88, area)
    } else {
        centered_rect(86, 78, area)
    };
    f.render_widget(Clear, popup_area);
    if reading {
        render_ask_reader(f, popup_area, app);
    } else {
        render_ask_panel(f, popup_area, app);
    }
}

/// The whole answer, scrollable. Everything the detail pane has to cut —
/// a long reply, a stack trace, a numbered plan — is here in full, and the
/// geometry is written back so the key handler can clamp the scroll.
fn render_ask_reader(f: &mut Frame, area: Rect, app: &mut App) {
    use std::fmt::Write as _;

    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some((id, scroll)) = app
        .ask_panel
        .reader
        .as_ref()
        .map(|reader| (reader.id.clone(), reader.scroll))
    else {
        return;
    };
    let Some(index) = app.ask_entries.iter().position(|entry| entry.id == id) else {
        // Deleted out from under the reader — by `D` elsewhere, or by the
        // daemon aging the history out. Fall back to the list rather than
        // paint a blank page.
        app.ask_panel.reader = None;
        app.set_hint("ask history: that entry is gone", HintLevel::Warn);
        return;
    };
    let entry = &app.ask_entries[index];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        // A gutter on both sides: prose set flush against a box rule is
        // the difference between a pane you skim and one you read.
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::styled(" answer ", theme.accent_badge()),
            ask_status_badge(entry, theme),
            Span::styled(format!(" {} ", entry.agent), theme.table_header_style()),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // One line at the foot for the scroll position: without it a reader
    // cannot tell a short answer from the top of a long one.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let width = usize::from(chunks[0].width).max(8);

    let mut meta = format!("{} · {}", entry.agent, entry.cwd);
    if let Some(d) = entry.duration() {
        let _ = write!(meta, " · {}s", d.as_secs());
    }
    if let Some(cost) = entry.cost_usd {
        let _ = write!(meta, " · ${cost:.4}");
    }
    let mut lines = vec![Line::from(Span::styled(
        truncate_chars(&meta, width),
        theme.dim_style(),
    ))];
    // No budget on either wrap: the reader is the surface that does not
    // truncate, which is why it exists.
    lines.extend(
        wrap_detail_text("ask: ", &entry.prompt, width, usize::MAX)
            .into_iter()
            .map(Line::from),
    );
    lines.push(Line::from(""));
    let body = match entry.status {
        muxa::ask::AskStatus::Running if entry.answer.is_empty() => {
            "…waiting for the agent".to_string()
        }
        muxa::ask::AskStatus::Failed => entry
            .error
            .clone()
            .unwrap_or_else(|| "failed with no detail".into()),
        muxa::ask::AskStatus::Running | muxa::ask::AskStatus::Answered => entry.answer.clone(),
    };
    // The detail pane paints an answer green to mark it as the reply; a
    // whole page of green is a wall to read. The status badge in the title
    // carries that signal here, so only a failure keeps its colour.
    let style = match entry.status {
        muxa::ask::AskStatus::Failed => Style::default().fg(Color::Red),
        muxa::ask::AskStatus::Running => theme.dim_style(),
        muxa::ask::AskStatus::Answered => Style::default(),
    };
    lines.extend(
        wrap_detail_text("", &body, width, usize::MAX)
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style))),
    );

    let total = lines.len();
    let view = usize::from(chunks[0].height).max(1);
    let scroll = scroll.min(total.saturating_sub(view));
    let visible: Vec<Line> = lines.into_iter().skip(scroll).take(view).collect();
    f.render_widget(Paragraph::new(visible), chunks[0]);

    let position = if total <= view {
        format!("{total} lines")
    } else {
        format!("{}–{} / {total}", scroll + 1, (scroll + view).min(total))
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(position, theme.dim_style())))
            .alignment(Alignment::Right),
        chunks[1],
    );

    if let Some(reader) = app.ask_panel.reader.as_mut() {
        reader.total_lines = total;
        reader.view_height = view;
        reader.scroll = scroll;
    }
}

fn ask_detail_lines(
    entry: &muxa::ask::AskEntry,
    width: usize,
    height: usize,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    use std::fmt::Write as _;
    let mut meta = format!("{} · {}", entry.agent, entry.cwd);
    if let Some(d) = entry.duration() {
        let _ = write!(meta, " · {}s", d.as_secs());
    }
    if let Some(cost) = entry.cost_usd {
        let _ = write!(meta, " · ${cost:.4}");
    }
    let mut lines = vec![Line::from(Span::styled(
        truncate_chars(&meta, width),
        theme.dim_style(),
    ))];
    // Question first, then the answer — the pane reads top-down like the
    // exchange happened. Both wrap; a truncated answer is the reason to
    // open this panel at all.
    let remaining = height.saturating_sub(1).max(2);
    let q_budget = remaining.clamp(1, 3);
    lines.extend(
        wrap_detail_text("ask: ", &entry.prompt, width, q_budget)
            .into_iter()
            .map(Line::from),
    );
    let body = match entry.status {
        muxa::ask::AskStatus::Running if entry.answer.is_empty() => {
            "…waiting for the agent".to_string()
        }
        muxa::ask::AskStatus::Failed => entry
            .error
            .clone()
            .unwrap_or_else(|| "failed with no detail".into()),
        muxa::ask::AskStatus::Running | muxa::ask::AskStatus::Answered => entry.answer.clone(),
    };
    let style = match entry.status {
        muxa::ask::AskStatus::Failed => Style::default().fg(Color::Red),
        muxa::ask::AskStatus::Running => theme.dim_style(),
        muxa::ask::AskStatus::Answered => Style::default().fg(Color::Green),
    };
    let a_budget = remaining.saturating_sub(q_budget).max(1);
    lines.extend(
        wrap_detail_text("", &body, width, a_budget)
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style))),
    );
    lines.truncate(height.max(1));
    lines
}

pub(super) fn render_spawn(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some(spawn) = app.spawn.as_ref() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(Span::styled(
            match spawn.target.as_ref() {
                Some(TopologyNodeKey::Session(_)) => " new window + first agent ",
                Some(TopologyNodeKey::Window(_) | TopologyNodeKey::Pane(_)) => {
                    " new sibling agent pane "
                }
                None => " new window + agent ",
            },
            theme.action_badge().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let avail = usize::from(inner.width.saturating_sub(9)).max(4);
    let focus_marker = |field: SpawnField| {
        if spawn.focus == field {
            Span::styled("> ", theme.action_badge())
        } else {
            Span::raw("  ")
        }
    };
    // dir and name stay single-line with a scrolling window; the prompt is
    // the field that grows, and it wraps *down* instead of clipping right —
    // a brief is a sentence or three, not a filename.
    let dir_view = truncate_prompt_input(&spawn.dir, avail);
    let name_line: Line = if !spawn.creates_window() {
        Line::from(vec![
            Span::raw("  "),
            Span::styled("window ", theme.dim_style()),
            Span::raw(truncate_prompt_input(&spawn.name, avail).text),
            Span::styled("  (selected)", theme.dim_style()),
        ])
    } else if spawn.name.is_empty() && spawn.focus != SpawnField::Name {
        Line::from(vec![
            focus_marker(SpawnField::Name),
            Span::styled("window ", theme.dim_style()),
            Span::styled(
                format!("(auto: {})", session_base_name(&spawn.dir)),
                theme.dim_style(),
            ),
        ])
    } else {
        Line::from(vec![
            focus_marker(SpawnField::Name),
            Span::styled("window ", theme.dim_style()),
            Span::raw(truncate_prompt_input(&spawn.name, avail).text),
        ])
    };
    let mut lines = vec![
        Line::from(vec![
            focus_marker(SpawnField::Dir),
            Span::styled("dir    ", theme.dim_style()),
            Span::raw(dir_view.text.clone()),
        ]),
        name_line,
        Line::from(vec![
            focus_marker(SpawnField::Agent),
            Span::styled("agent  ", theme.dim_style()),
            Span::raw("◂ "),
            Span::styled(format!(" {} ", spawn.agent.label()), theme.action_badge()),
            Span::raw(" ▸"),
        ]),
    ];
    let prompt_rows = wrap_input_rows(&spawn.prompt, avail);
    for (i, (row, _)) in prompt_rows.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                focus_marker(SpawnField::Prompt),
                Span::styled("prompt ", theme.dim_style()),
                Span::raw(row.clone()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::raw(row.clone()),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
    place_spawn_cursor(f, inner, spawn, &dir_view, &prompt_rows, avail);
}

/// Cursor for the focused spawn field, prompt rows included.
fn place_spawn_cursor(
    f: &mut Frame,
    inner: Rect,
    spawn: &SpawnComposer,
    dir_view: &VisiblePromptInput,
    prompt_rows: &[(String, usize)],
    avail: usize,
) {
    use unicode_width::UnicodeWidthStr;
    let (y, x) = match spawn.focus {
        SpawnField::Agent => return,
        SpawnField::Dir => {
            let before: String = dir_view
                .text
                .chars()
                .take(spawn.dir_cursor.saturating_sub(dir_view.skipped_chars))
                .collect();
            (
                inner.y,
                inner.x + 9 + u16::try_from(before.width()).unwrap_or(u16::MAX),
            )
        }
        SpawnField::Name => {
            let view = truncate_prompt_input(&spawn.name, avail);
            let before: String = view
                .text
                .chars()
                .take(spawn.name_cursor.saturating_sub(view.skipped_chars))
                .collect();
            (
                inner.y + 1,
                inner.x + 9 + u16::try_from(before.width()).unwrap_or(u16::MAX),
            )
        }
        SpawnField::Prompt => {
            // Locate the wrapped row containing the cursor.
            let mut y = inner.y + 3;
            let mut x = inner.x + 9;
            for (i, (row, start)) in prompt_rows.iter().enumerate() {
                let end = start + row.chars().count();
                let last = i + 1 == prompt_rows.len();
                if spawn.prompt_cursor >= *start && (spawn.prompt_cursor < end || last) {
                    let before: String = row.chars().take(spawn.prompt_cursor - start).collect();
                    y = inner.y + 3 + u16::try_from(i).unwrap_or(0);
                    x = inner.x + 9 + u16::try_from(before.width()).unwrap_or(u16::MAX);
                    break;
                }
            }
            (y, x)
        }
    };
    if x < inner.x + inner.width && y < inner.y + inner.height {
        f.set_cursor_position((x, y));
    }
}

pub(super) fn render_collaboration_composer(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;

    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let composer = app
        .collaboration_composer
        .as_ref()
        .expect("render_collaboration_composer without composer");
    if let Some(palette) = composer.skill_palette.as_ref() {
        render_message_skill_palette(f, area, app, &composer.label, palette, theme);
        return;
    }
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

fn render_message_skill_palette(
    f: &mut Frame,
    area: Rect,
    app: &App,
    target: &str,
    palette: &MessageSkillPalette,
    theme: WatchThemeSpec,
) {
    use unicode_width::UnicodeWidthStr;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(Line::from(vec![
            Span::styled(" skills ", theme.action_badge()),
            Span::styled(
                format!(" → {target} · text only · Enter inserts "),
                theme.table_header_style(),
            ),
        ]));
    let inner = block.inner(area);
    let visible_query =
        truncate_prompt_input(&palette.query, inner.width.saturating_sub(2) as usize);
    let matches = matching_skills(&app.message_skills, &palette.query);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("/ ", theme.accent_badge()),
            Span::raw(visible_query.text.clone()),
        ]),
        Line::from(""),
    ];
    let available = usize::from(inner.height).saturating_sub(lines.len());
    if app.message_skills.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no skills · F2 add · or muxa skill add <name> <prompt>",
            theme.dim_style().add_modifier(Modifier::ITALIC),
        )));
    } else if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching skill",
            theme.dim_style().add_modifier(Modifier::ITALIC),
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
                theme.selected_style()
            } else {
                theme.table_header_style()
            };
            let prefix = format!("  /{name}");
            let prompt_width = usize::from(inner.width)
                .saturating_sub(prefix.width())
                .saturating_sub(3);
            let summary = prompt.lines().next().unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled("  ", theme.dim_style()),
                Span::styled(
                    truncate_prompt_input(summary, prompt_width).text,
                    theme.dim_style(),
                ),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);

    let cursor_x = inner
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(visible_query.text.width()).unwrap_or(u16::MAX));
    if inner.height > 0 && cursor_x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((cursor_x, inner.y));
    }
}

pub(super) fn render_message_skill_editor(f: &mut Frame, area: Rect, app: &App) {
    use unicode_width::UnicodeWidthStr;

    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let Some(editor) = app.message_skill_editor.as_ref() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.action))
        .border_type(theme.border_type)
        .title(Line::from(vec![
            Span::styled(" skill add/update ", theme.action_badge()),
            Span::styled(
                " text only · keeps kind/mode/ask permissions ",
                theme.dim_style(),
            ),
        ]));
    let inner = block.inner(area);
    let field_width = usize::from(inner.width.saturating_sub(10));
    let name = truncate_prompt_input(&editor.name, field_width);
    let prompt = truncate_prompt_input(&editor.prompt, field_width);
    let name_style = if editor.field == MessageSkillEditorField::Name {
        theme.selected_style()
    } else {
        theme.table_header_style()
    };
    let prompt_style = if editor.field == MessageSkillEditorField::Prompt {
        theme.selected_style()
    } else {
        theme.table_header_style()
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(" name   ", name_style),
            Span::raw("/"),
            Span::raw(name.text.clone()),
        ]),
        Line::from(vec![
            Span::styled(" prompt ", prompt_style),
            Span::raw(prompt.text.clone()),
        ]),
        Line::from(Span::styled(
            " Tab field · Enter next/save · Esc back",
            theme.dim_style(),
        )),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);

    let (row, prefix, visible, cursor, skipped) = match editor.field {
        MessageSkillEditorField::Name => (0, 9, &name.text, editor.name_cursor, name.skipped_chars),
        MessageSkillEditorField::Prompt => (
            1,
            8,
            &prompt.text,
            editor.prompt_cursor,
            prompt.skipped_chars,
        ),
    };
    let before: String = visible
        .chars()
        .take(cursor.saturating_sub(skipped))
        .collect();
    let x = inner
        .x
        .saturating_add(prefix)
        .saturating_add(u16::try_from(before.width()).unwrap_or(u16::MAX));
    let y = inner.y.saturating_add(row);
    if x < inner.x.saturating_add(inner.width) && y < inner.y.saturating_add(inner.height) {
        f.set_cursor_position((x, y));
    }
}

pub(super) fn render_broadcast_report_overlay(f: &mut Frame, area: Rect, app: &App) {
    if app.broadcast_report.is_none() {
        return;
    }
    let popup_area = centered_rect(72, 60, area);
    f.render_widget(Clear, popup_area);
    render_broadcast_report(f, popup_area, app);
}

/// One row per recipient: the request id it got, or why it did not.
fn render_broadcast_report(f: &mut Frame, area: Rect, app: &App) {
    let Some(report) = app.broadcast_report.as_ref() else {
        return;
    };
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let border = if report.failed() == 0 {
        theme.state_idle
    } else {
        theme.state_error
    };
    let mut lines: Vec<Line> = report
        .rows
        .iter()
        .map(|row| match &row.outcome {
            Ok(id) => Line::from(vec![
                Span::styled("  ok   ", Style::default().fg(theme.state_idle)),
                Span::raw(format!("{:<28} ", row.label)),
                Span::styled(id.clone(), theme.dim_style()),
            ]),
            Err(error) => Line::from(vec![
                Span::styled("  fail ", Style::default().fg(theme.state_error)),
                Span::raw(format!("{:<28} ", row.label)),
                Span::styled(error.clone(), Style::default().fg(theme.state_error)),
            ]),
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" Esc ", theme.key_badge()),
        Span::styled(
            "dismiss · a failed recipient was not sent to, and no retry happened",
            theme.dim_style(),
        ),
    ]));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(format!(" {} ", report.title())));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn collaboration_composer_title(
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
        | CollaborationComposeTarget::Prompt { .. }
        | CollaborationComposeTarget::PromptTopology { .. }
        | CollaborationComposeTarget::PromptPanes { .. } => (
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
        CollaborationComposeTarget::Send { kind, mode, .. }
        | CollaborationComposeTarget::Broadcast { kind, mode, .. } => {
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
