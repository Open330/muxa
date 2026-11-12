use super::{
    collab_screen, collaboration_open_hint, collaboration_requests, request_kind_label,
    selected_collaboration_request, truncate_chars, watch_theme, work_mode_label, App, Block,
    Borders, CollaborationMailboxTab, CollaborationRequest, Color, Constraint, Direction, Frame,
    HashMap, Layout, Line, MailboxDetail, MailboxHistoryScope, Modifier, OffsetDateTime, Paragraph,
    Participant, Rect, RequestStatus, Span, Style, WatchThemeSpec,
};

pub(super) fn render_collaboration_mailbox(f: &mut Frame, area: Rect, app: &App) {
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
                Line::from(Span::styled("Esc/M closes · b alias", theme.dim_style())),
            ]),
            inner,
        );
        return;
    }

    let detail_height = match app.collaboration_mailbox.detail_expand {
        MailboxDetail::Compact => 6,
        MailboxDetail::Half => inner.height / 2,
        // Leave the list three rows: enough to keep selection context
        // without stealing the reading space the user just asked for.
        MailboxDetail::Most => inner.height.saturating_sub(3),
    }
    .max(3);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
        .split(inner);
    let max_lines = usize::from(chunks[0].height).max(1);
    let width = usize::from(chunks[0].width).saturating_sub(2);
    let lines = collaboration_mailbox_request_lines(app, width, max_lines, theme);
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let detail_title = selected_collaboration_request(app).map_or_else(
        || Line::from(Span::styled(" conversation ", theme.dim_style())),
        |request| {
            Line::from(Span::styled(
                format!(
                    " conversation · {} → {} ",
                    request.from.label(),
                    request.to.label()
                ),
                theme.table_header_style(),
            ))
        },
    );
    let detail_block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border_style())
        .title(detail_title);
    let detail_inner = detail_block.inner(chunks[1]);
    let detail_width = usize::from(detail_inner.width).max(1);
    let detail_height = usize::from(detail_inner.height).max(1);
    let detail = selected_collaboration_request(app).map_or_else(
        || vec![Line::from(Span::styled("-", theme.dim_style()))],
        |request| collaboration_request_detail_lines(request, detail_width, detail_height, theme),
    );
    f.render_widget(Paragraph::new(detail).block(detail_block), chunks[1]);
}

fn collaboration_mailbox_title(app: &App, theme: WatchThemeSpec) -> Line<'static> {
    let tab = app.collaboration_mailbox.tab;
    if tab == CollaborationMailboxTab::Human {
        return Line::from(vec![
            Span::styled(
                format!(" need your response {} ", app.collaboration.human.len()),
                theme.action_badge(),
            ),
            Span::raw(" Tab: agent traffic · e: reply "),
        ]);
    }
    let owner = app
        .collaboration
        .history_scope
        .as_ref()
        .map_or("no topology scope selected", MailboxHistoryScope::label);
    let read_only = app
        .collaboration
        .history_scope
        .as_ref()
        .is_some_and(MailboxHistoryScope::aggregate);
    let mut spans = vec![
        Span::styled(" messages ", theme.accent_badge()),
        Span::styled(
            format!(
                " need you {} · Tab to switch ",
                app.collaboration.human.len()
            ),
            if tab == CollaborationMailboxTab::Human {
                theme.action_badge()
            } else {
                theme.dim_style()
            },
        ),
        Span::raw(" "),
    ];
    if read_only {
        spans.extend([
            Span::styled(format!(" {owner} "), theme.table_header_style()),
            Span::raw(" "),
            Span::styled(
                format!(" {} in history ", app.collaboration.incoming.len()),
                theme.action_badge(),
            ),
        ]);
    } else {
        spans.extend([
            Span::styled(
                format!(" incoming {} · {owner} ", app.collaboration.incoming.len()),
                if tab == CollaborationMailboxTab::Incoming {
                    theme.action_badge()
                } else {
                    theme.dim_style()
                },
            ),
            Span::raw(" "),
            Span::styled(
                format!(" sent {} · console ", app.collaboration.sent.len()),
                if tab == CollaborationMailboxTab::Sent {
                    theme.action_badge()
                } else {
                    theme.dim_style()
                },
            ),
        ]);
    }
    Line::from(spans)
}

pub(super) fn collaboration_mailbox_request_lines(
    app: &App,
    width: usize,
    max_lines: usize,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    let requests = collaboration_requests(app);
    let selected = app.collaboration_mailbox.selected;
    let tab = app.collaboration_mailbox.tab;
    let group_windows = matches!(
        app.collaboration.history_scope.as_ref(),
        Some(MailboxHistoryScope::Session { .. })
    );
    let aggregate = app
        .collaboration
        .history_scope
        .as_ref()
        .is_some_and(MailboxHistoryScope::aggregate);
    // Retain request indexes beside painted lines because window headers are
    // not selectable. The viewport can then follow the selected request even
    // when a header consumes one of its rows.
    let mut painted: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    if group_windows {
        for (window, requests) in session_mailbox_window_groups(app, requests) {
            painted.push((
                None,
                Line::from(Span::styled(
                    format!(
                        "  {window} · {} message{}",
                        requests.len(),
                        if requests.len() == 1 { "" } else { "s" }
                    ),
                    theme.table_header_style(),
                )),
            ));
            for (index, request) in requests {
                let lines = collaboration_mailbox_request_lines_for_request(
                    request,
                    index == selected,
                    tab,
                    aggregate,
                    width,
                    theme,
                );
                let mut lines = lines.into_iter();
                if let Some(first) = lines.next() {
                    painted.push((Some(index), first));
                }
                painted.extend(lines.map(|line| (None, line)));
            }
        }
    } else {
        for (index, request) in requests.iter().enumerate() {
            let lines = collaboration_mailbox_request_lines_for_request(
                request,
                index == selected,
                tab,
                aggregate,
                width,
                theme,
            );
            let mut lines = lines.into_iter();
            if let Some(first) = lines.next() {
                painted.push((Some(index), first));
            }
            painted.extend(lines.map(|line| (None, line)));
        }
    }
    if painted.is_empty() {
        return vec![Line::from(Span::styled(
            match tab {
                CollaborationMailboxTab::Human => "no requests need your response",
                CollaborationMailboxTab::Incoming => "no incoming requests",
                CollaborationMailboxTab::Sent => "no sent requests",
            },
            theme.dim_style(),
        ))];
    }
    let selected_line = painted
        .iter()
        .position(|(index, _)| *index == Some(selected))
        .unwrap_or(0);
    let start = selected_line
        .saturating_add(1)
        .saturating_sub(max_lines)
        .min(painted.len().saturating_sub(max_lines));
    painted
        .into_iter()
        .skip(start)
        .take(max_lines)
        .map(|(_, line)| line)
        .collect()
}

fn mailbox_scope_participant<'a>(app: &App, request: &'a CollaborationRequest) -> &'a Participant {
    app.collaboration
        .history_scope
        .as_ref()
        .filter(|scope| scope.matches(&request.from))
        .map_or(&request.to, |_| &request.from)
}

fn session_mailbox_window_groups<'a>(
    app: &App,
    requests: &'a [CollaborationRequest],
) -> Vec<(String, Vec<(usize, &'a CollaborationRequest)>)> {
    let mut groups: Vec<(String, Vec<(usize, &CollaborationRequest)>)> = Vec::new();
    let mut indexes = HashMap::<String, usize>::new();
    for (index, request) in requests.iter().enumerate() {
        let participant = mailbox_scope_participant(app, request);
        let stable = format!(
            "{}\u{1f}{}\u{1f}{}",
            participant.room.host,
            participant.room.socket.as_deref().unwrap_or_default(),
            participant.room.window_id
        );
        let label = participant
            .window_name
            .clone()
            .unwrap_or_else(|| participant.room.window_id.clone());
        let group = *indexes.entry(stable).or_insert_with(|| {
            groups.push((label, Vec::new()));
            groups.len() - 1
        });
        groups[group].1.push((index, request));
    }
    let mut label_counts = HashMap::<String, usize>::new();
    for (label, _) in &groups {
        *label_counts.entry(label.clone()).or_default() += 1;
    }
    groups
        .into_iter()
        .map(|(label, requests)| {
            let duplicate = label_counts.get(&label).copied().unwrap_or_default() > 1;
            let label = if duplicate {
                let window_id = requests
                    .first()
                    .map(|(_, request)| {
                        mailbox_scope_participant(app, request)
                            .room
                            .window_id
                            .as_str()
                    })
                    .unwrap_or_default();
                format!("{label} · {window_id}")
            } else {
                label
            };
            (label, requests)
        })
        .collect()
}

fn collaboration_mailbox_request_lines_for_request(
    request: &CollaborationRequest,
    focused: bool,
    tab: CollaborationMailboxTab,
    aggregate: bool,
    width: usize,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    let peer = if aggregate {
        format!("{} → {}", request.from.label(), request.to.label())
    } else {
        match tab {
            CollaborationMailboxTab::Human | CollaborationMailboxTab::Incoming => {
                request.from.label()
            }
            CollaborationMailboxTab::Sent => request.to.label(),
        }
    };
    let body = request
        .body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let age = collab_screen::age(OffsetDateTime::now_utc(), request.created_at);
    let (status_icon, status_label) = mailbox_status(request.status);
    let summary = truncate_chars(
        &format!(
            "{status_icon} {peer}  ·  {status_label}  ·  {}  ·  {age} ago",
            request_kind_label(request.kind)
        ),
        width.saturating_sub(2),
    );
    let marker = Span::styled(
        if focused { "> " } else { "  " },
        if focused {
            Style::default().fg(theme.action)
        } else {
            theme.dim_style()
        },
    );
    let row_style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    vec![
        Line::from(vec![marker, Span::styled(summary, row_style)]),
        Line::from(Span::styled(
            truncate_chars(&format!("    {body}"), width),
            if focused {
                row_style
            } else {
                theme.dim_style()
            },
        )),
    ]
}

fn mailbox_status(status: RequestStatus) -> (&'static str, &'static str) {
    match status {
        RequestStatus::Queued => ("○", "waiting"),
        RequestStatus::Claimed => ("●", "in progress"),
        RequestStatus::Completed => ("✓", "done"),
        RequestStatus::Blocked => ("!", "blocked"),
        RequestStatus::Declined => ("–", "declined"),
        RequestStatus::Failed => ("×", "failed"),
        RequestStatus::Expired => ("×", "expired"),
        RequestStatus::Cancelled => ("×", "cancelled"),
    }
}

/// Wrap `prefix + text` into at most `max_lines` rows of `width` chars,
/// ellipsizing the last row when the text goes on. Hard char-chunking, not
/// word wrap: request bodies are prose in any language and a `fold`-style
/// break is predictable, cheap, and never loses a row to a long token.
/// Char-counted like `truncate_chars`; the pane's other rows already
/// accept that approximation for wide glyphs.
pub(super) fn wrap_detail_text(
    prefix: &str,
    text: &str,
    width: usize,
    max_lines: usize,
) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(8);
    let max_lines = max_lines.max(1);

    // The author's line breaks are content. Collapsing them into spaces —
    // which this did — turns a bulleted answer or a code block into one
    // grey wall, which is precisely the thing the detail pane exists to
    // let you read. Only *runs* of blank lines collapse, to a single one,
    // so a markdown reply keeps its shape without spending the pane on
    // vertical whitespace.
    let mut rows: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut blank_run = false;
    'outer: for (i, source_line) in text.split('\n').enumerate() {
        let source_line = source_line.trim_end();
        if source_line.trim().is_empty() {
            if blank_run || rows.is_empty() {
                continue;
            }
            blank_run = true;
            if rows.len() == max_lines {
                truncated = true;
                break;
            }
            rows.push(String::new());
            continue;
        }
        blank_run = false;
        // The prefix belongs to the first visual row only; continuation
        // rows align under it so the label reads as a gutter, not as part
        // of the text.
        let mut pending: String = if i == 0 {
            format!("{prefix}{source_line}")
        } else {
            format!("{}{source_line}", " ".repeat(prefix.chars().count()))
        };
        while !pending.is_empty() {
            if rows.len() == max_lines {
                truncated = true;
                break 'outer;
            }
            let mut taken = String::new();
            let mut used = 0usize;
            let mut rest = String::new();
            for c in pending.chars() {
                let w = c.width().unwrap_or(0);
                if !rest.is_empty() || used + w > width {
                    rest.push(c);
                } else {
                    used += w;
                    taken.push(c);
                }
            }
            rows.push(taken);
            pending = rest;
        }
    }

    if truncated {
        if let Some(last) = rows.last_mut() {
            last.pop();
            last.push('…');
        }
    }
    if rows.is_empty() {
        rows.push(prefix.trim_end().to_string());
    }
    rows
}

fn collaboration_request_detail_lines(
    request: &CollaborationRequest,
    width: usize,
    height: usize,
    theme: WatchThemeSpec,
) -> Vec<Line<'static>> {
    let (_, status) = mailbox_status(request.status);
    let mut lines = vec![Line::from(Span::styled(
        truncate_chars(
            &format!(
                "{} · {status} · {} · {} ago",
                request_kind_label(request.kind),
                work_mode_label(request.work_mode),
                collab_screen::age(OffsetDateTime::now_utc(), request.created_at)
            ),
            width,
        ),
        theme.dim_style(),
    ))];
    // Human-readable content owns the compact view. Stable ids and causal
    // metadata only appear after `|` expands the detail pane.
    let show_technical = height >= 10;
    let remaining = height
        .saturating_sub(lines.len() + usize::from(show_technical))
        .max(2);
    let (body_budget, reply_budget) = if request.reply.is_some() {
        let body = remaining.div_ceil(2);
        (body, remaining - body)
    } else {
        (remaining, 0)
    };
    lines.extend(
        wrap_detail_text("Request: ", &request.body, width, body_budget)
            .into_iter()
            .map(Line::from),
    );
    if let Some(reply) = request.reply.as_ref() {
        let (_, reply_status) = mailbox_status(reply.status);
        let prefix = format!("Reply ({reply_status}): ");
        lines.extend(
            wrap_detail_text(&prefix, &reply.body, width, reply_budget.max(1))
                .into_iter()
                .map(|row| Line::from(Span::styled(row, Style::default().fg(Color::Green)))),
        );
    }
    if show_technical {
        let work = match (request.workspace_id.as_deref(), request.work_id.as_deref()) {
            (Some(workspace), Some(work)) => format!(" · work {workspace}/{work}"),
            (None, Some(work)) => format!(" · work {work}"),
            _ => String::new(),
        };
        let parent = request
            .parent_request_id
            .as_deref()
            .map_or_else(String::new, |parent| format!(" · parent {parent}"));
        let run = request
            .run_id
            .as_deref()
            .map_or_else(String::new, |run| format!(" · run {run}"));
        lines.push(Line::from(Span::styled(
            truncate_chars(
                &format!(
                    "id {} · thread {}{parent}{work}{run}",
                    request.id,
                    request.thread_id.as_deref().unwrap_or(&request.id),
                ),
                width,
            ),
            theme.dim_style(),
        )));
    }
    lines.truncate(height.max(1));
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("-", theme.dim_style())));
    }
    lines
}

/// Prompt input should feel like a command bar, not a modal dialog: it
/// hugs the bottom of the current content area and consumes only the
/// border plus one editable line.
pub(super) fn bottom_prompt_rect(r: Rect) -> Rect {
    let height = r.height.min(3);
    Rect {
        x: r.x,
        y: r.y.saturating_add(r.height.saturating_sub(height)),
        width: r.width,
        height,
    }
}
