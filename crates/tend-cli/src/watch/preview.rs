use super::{
    pane_display, preview_label, preview_position, watch_theme, App, Block, Borders, Color, Frame,
    Line, Modifier, PaneKey, Paragraph, PreviewContent, Rect, Span, Style, TopologyNodeKey,
    TopologyNodeRef, WatchRow,
};

pub(super) fn render_preview(f: &mut Frame, area: Rect, app: &App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let preview = app
        .preview
        .as_ref()
        .expect("render_preview without preview");

    let mode_tag = match preview.content {
        PreviewContent::PromptResponse => "prompt",
        PreviewContent::LivePane => "live",
    };
    let target_tag = preview_position(app, preview)
        .filter(|(_, total)| *total > 1)
        .map(|(idx, total)| format!(" · {idx}/{total}"))
        .unwrap_or_default();
    let title = format!(
        " preview · {} · {}{} ",
        preview_label(app, preview),
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
        let body = build_pane_capture_body_on(
            app,
            &preview.pane_id,
            preview
                .pane_key
                .as_ref()
                .map(|key| key.window.session.endpoint.socket.as_str()),
        );
        let paragraph = Paragraph::new(body)
            .block(block)
            .scroll((preview.scroll, 0));
        f.render_widget(paragraph, area);
        return;
    }

    let lines = build_preview_lines_for(app, &preview.pane_id, preview.pane_key.as_ref());
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
pub(super) fn build_pane_capture_body<'a>(app: &'a App, pane_id: &str) -> ratatui::text::Text<'a> {
    build_pane_capture_body_on(app, pane_id, None)
}

pub(super) fn build_pane_capture_body_on<'a>(
    app: &'a App,
    pane_id: &str,
    socket: Option<&str>,
) -> ratatui::text::Text<'a> {
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
    if cached.pane_id != pane_id || cached.socket.as_deref() != socket {
        // Stale entry from a previous selection — the main loop will
        // re-fetch on the next iteration.
        return placeholder("(capturing pane…)");
    }
    if cached.text.is_empty() {
        return placeholder("(pane gone or capture failed)");
    }
    cached
        .parsed
        .clone()
        .unwrap_or_else(|| placeholder("(could not parse pane content)"))
}

/// Compose the textual body of the preview pane. Pulled out so unit tests
/// can assert on the rendered structure without going through ratatui.
#[cfg(test)]
pub(super) fn build_preview_lines<'a>(app: &'a App, pane_id: &str) -> Vec<Line<'a>> {
    build_preview_lines_for(app, pane_id, None)
}

fn build_preview_lines_for<'a>(
    app: &'a App,
    pane_id: &str,
    pane_key: Option<&PaneKey>,
) -> Vec<Line<'a>> {
    let mut out: Vec<Line<'a>> = Vec::new();

    let agent = pane_key
        .and_then(|key| app.topology.find(&TopologyNodeKey::Pane(key.clone())))
        .and_then(|node| match node {
            TopologyNodeRef::Pane(pane) => pane.agent.as_ref(),
            TopologyNodeRef::Session(_) | TopologyNodeRef::Window(_) => None,
        })
        .or_else(|| {
            if pane_key.is_some() {
                return None;
            }
            app.rows.iter().find_map(|r| match r {
                WatchRow::Agent(a) if a.pane.as_deref() == Some(pane_id) => Some(a.as_ref()),
                WatchRow::Work(s) => s
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
            })
        });

    let pane_label = pane_key.map_or_else(
        || pane_display(Some(pane_id), &app.panes),
        |key| {
            format!(
                "{}:{} {}",
                key.window.session.endpoint.host, key.window.session.endpoint.socket, key.pane_id
            )
        },
    );
    let pane_socket = pane_key
        .map(|key| key.window.session.endpoint.socket.as_str())
        .or_else(|| agent.and_then(|agent| agent.tmux_socket.as_deref()));
    if let Some(agent) = agent {
        // Header line — pane label + kind + state, all on one row.
        out.push(Line::from(vec![
            Span::styled("pane: ", Style::default().add_modifier(Modifier::DIM)),
            // The route label may be a location rather than a raw pane ID.
            // Only address occurrences receive identity colors.
            Span::styled(
                pane_label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
            Span::styled("kind: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(agent.kind.to_string()),
            Span::raw("    "),
            Span::styled("state: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(agent.state.to_string()),
        ]));
        if let Some(header) = out.pop() {
            out.push(crate::pane_identity::highlight_line(
                header,
                pane_id,
                pane_socket,
            ));
        }
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
