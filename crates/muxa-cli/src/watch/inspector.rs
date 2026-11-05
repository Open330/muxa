use super::{
    agent_kind_short, agent_needs_attention, build_pane_capture_body_on, format_duration,
    icons_unicode, participant_matches_window, relative_time, selected_tree_target_from,
    state_age_label, state_marker, state_sort_rank, state_summary_spans, tree_node_states,
    truncate_chars, Agent, App, Block, BorderType, Borders, CapturedWindow, CapturedWindowPane,
    Clear, Frame, Line, Modifier, OffsetDateTime, PaneGeometry, PaneNode, Paragraph, Rect,
    SessionNode, Span, Spinner, StateDistribution, Style, TopologyNodeRef, TreeTarget,
    WatchThemeSpec, WindowNode, WorkloadProcessKind, WINDOW_MOSAIC_MIN_HEIGHT,
    WINDOW_MOSAIC_MIN_WIDTH,
};

fn state_distribution_label(states: &StateDistribution) -> String {
    let mut labels = Vec::new();
    for (label, count) in [
        ("error", states.error),
        ("input", states.waiting_input),
        ("choice", states.waiting_choice),
        ("working", states.working),
        ("starting", states.starting),
        ("idle", states.idle),
        ("stopped", states.stopped),
    ] {
        if count > 0 {
            labels.push(format!("{label} {count}"));
        }
    }
    if labels.is_empty() {
        "no agents".into()
    } else {
        labels.join(" · ")
    }
}

fn topology_inspector_status_line(
    node: TopologyNodeRef<'_>,
    app: &App,
    theme: WatchThemeSpec,
) -> Line<'static> {
    let states = tree_node_states(node);
    let label = match node {
        TopologyNodeRef::Session(session) => state_distribution_label(&session.states),
        TopologyNodeRef::Window(window) => state_distribution_label(&window.states),
        TopologyNodeRef::Pane(pane) => pane
            .agent
            .as_ref()
            .map_or_else(|| "untracked".into(), |agent| agent.state.to_string()),
    };
    let mut spans = vec![Span::styled("status  ", theme.dim_style())];
    if states.is_empty() {
        spans.push(Span::styled("○", theme.dim_style()));
    } else {
        spans.extend(state_summary_spans(
            states,
            theme,
            Spinner {
                frame: app.anim_frame,
                enabled: app.watch_cfg.spinner && icons_unicode(),
            },
        ));
    }
    spans.push(Span::raw(format!("  {label}")));
    Line::from(spans)
}

/// The activity ledger predates endpoint-aware topology and is keyed only by
/// native session id. Never project one ledger value onto two sockets that
/// happen to reuse `$N`; absence is more honest than a plausible wrong age.
pub(super) fn unambiguous_session_activity_secs(
    app: &App,
    session: &SessionNode,
    now: OffsetDateTime,
) -> Option<u64> {
    let topology_matches = app
        .topology
        .sessions
        .iter()
        .filter(|candidate| candidate.key.session_id == session.key.session_id)
        .count();
    if topology_matches != 1 {
        return None;
    }
    app.session_activity
        .iter()
        .filter(|activity| activity.session_id == session.key.session_id)
        .map(|activity| activity.effective_total_secs(now))
        .max()
}

fn inspector_rule(width: u16, theme: WatchThemeSpec) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(usize::from(width.saturating_sub(2))),
        theme.dim_style(),
    ))
}

pub(super) fn inspector_agent_summary(agent: &Agent) -> String {
    muxa::prompt_headline(
        agent
            .last_notification
            .as_deref()
            .or(agent.summary_text())
            .or(agent.last_prompt.as_deref())
            .unwrap_or("—"),
    )
    .replace('\n', " ")
}

fn inspector_latest_agent<'a>(
    panes: impl Iterator<Item = &'a PaneNode>,
) -> Option<(&'a PaneNode, &'a Agent)> {
    panes
        .filter_map(|pane| pane.agent.as_ref().map(|agent| (pane, agent)))
        .max_by_key(|(_, agent)| agent.last_activity_at)
}

fn inspector_priority_agent<'a>(
    panes: impl Iterator<Item = &'a PaneNode>,
) -> Option<(&'a PaneNode, &'a Agent)> {
    panes
        .filter_map(|pane| pane.agent.as_ref().map(|agent| (pane, agent)))
        .min_by(|(_, left), (_, right)| {
            state_sort_rank(left.state)
                .cmp(&state_sort_rank(right.state))
                .then_with(|| left.state_entered_at.cmp(&right.state_entered_at))
                .then_with(|| right.last_activity_at.cmp(&left.last_activity_at))
        })
}

#[derive(Default)]
struct InspectorLoad {
    processes: u32,
    shells: u32,
    subagents: u32,
    max_context: Option<f32>,
    cost: f64,
    has_cost: bool,
}

fn inspector_load(panes: &[PaneNode]) -> InspectorLoad {
    let mut load = InspectorLoad::default();
    for agent in panes.iter().filter_map(|pane| pane.agent.as_ref()) {
        load.processes += u32::from(agent.workload.process_count);
        load.shells += u32::from(agent.workload.shell_count);
        load.subagents += u32::from(agent.workload.subagent_count)
            .max(u32::try_from(agent.subagents.len()).unwrap_or(u32::MAX));
        if let Some(context) = agent.context_used_pct {
            load.max_context = Some(load.max_context.map_or(context, |old| old.max(context)));
        }
        if let Some(cost) = agent.cost_usd {
            load.cost += cost;
            load.has_cost = true;
        }
    }
    load
}

fn inspector_field_line(
    label: &'static str,
    value: String,
    width: usize,
    theme: WatchThemeSpec,
) -> Line<'static> {
    let value_width = width.saturating_sub(label.chars().count() + 2);
    Line::from(vec![
        Span::styled(format!("{label}  "), theme.dim_style()),
        Span::raw(truncate_chars(&value, value_width)),
    ])
}

fn inspector_attention_line(
    location: String,
    agent: &Agent,
    now: OffsetDateTime,
    width: usize,
    theme: WatchThemeSpec,
) -> Line<'static> {
    let value = format!(
        "{location} · {} {} · {}",
        state_age_label(agent.state),
        relative_time(agent.state_entered_at, now),
        inspector_agent_summary(agent)
    );
    let value_width = width.saturating_sub(11);
    Line::from(vec![
        Span::styled("attention  ", theme.state_style(agent.state)),
        Span::styled(
            truncate_chars(&value, value_width),
            theme.state_style(agent.state),
        ),
    ])
}

fn inspector_window_roster_line(
    window: &WindowNode,
    width: usize,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Line<'static> {
    let priority = inspector_priority_agent(window.panes.iter());
    let latest = inspector_latest_agent(window.panes.iter());
    let name = format!("{}:{}", window.index, window.name);
    let counts = format!("{}/{}", window.states.total(), window.panes.len());
    let summary = latest.map_or_else(
        || "no tracked agents".into(),
        |(_, agent)| inspector_agent_summary(agent),
    );
    let (glyph, style, state, age) = priority.map_or_else(
        || ("○", theme.dim_style(), "—", "—".into()),
        |(_, agent)| {
            let (glyph, style) = state_marker(agent.state, theme, spin);
            (
                glyph,
                style,
                state_age_label(agent.state),
                relative_time(agent.state_entered_at, now),
            )
        },
    );
    let name_width = if width >= 68 { 18 } else { 12 };
    let body = if width >= 56 {
        format!(" {name:<name_width$} {counts:>5} {state:<6} {age:<4} {summary}")
    } else {
        format!(" {name:<name_width$} {counts:>5} {state:<6} {summary}")
    };
    Line::from(vec![
        Span::styled(glyph.to_string(), style),
        Span::raw(truncate_chars(&body, width.saturating_sub(1))),
    ])
}

pub(super) fn inspector_pane_roster_line(
    pane: &PaneNode,
    width: usize,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Line<'static> {
    inspector_pane_roster_line_with_prefix(pane, width, now, theme, spin, "")
}

fn inspector_pane_roster_line_with_prefix(
    pane: &PaneNode,
    width: usize,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
    prefix: &'static str,
) -> Line<'static> {
    let prefix_width = prefix.chars().count();
    let body_width = width.saturating_sub(prefix_width.saturating_add(1));
    let Some(agent) = pane.agent.as_ref() else {
        let body = format!(
            " {:<5} {:<8} {} · {}",
            pane.key.pane_id, pane.current_command, pane.title, pane.cwd
        );
        return crate::pane_identity::highlight_line(
            Line::from(vec![
                Span::styled(prefix, theme.dim_style()),
                Span::styled("○", theme.dim_style()),
                Span::styled(truncate_chars(&body, body_width), theme.dim_style()),
            ]),
            &pane.key.pane_id,
            Some(&pane.key.window.session.endpoint.socket),
        );
    };

    let (glyph, style) = state_marker(agent.state, theme, spin);
    let age = relative_time(agent.state_entered_at, now);
    let summary = inspector_agent_summary(agent);
    let body = if width >= 68 {
        let model = agent.model.as_deref().unwrap_or("—");
        let context = agent
            .context_used_pct
            .map_or_else(|| "—".into(), |value| format!("{value:.0}%"));
        format!(
            " {:<5} {:<8} {:<6} {:<4} {:<10} {:>4} {}",
            pane.key.pane_id,
            agent_kind_short(agent.kind),
            state_age_label(agent.state),
            age,
            model,
            context,
            summary
        )
    } else {
        format!(
            " {:<5} {:<8} {:<6} {:<4} {}",
            pane.key.pane_id,
            agent_kind_short(agent.kind),
            state_age_label(agent.state),
            age,
            summary
        )
    };
    crate::pane_identity::highlight_line(
        Line::from(vec![
            Span::styled(prefix, theme.dim_style()),
            Span::styled(glyph.to_string(), style),
            Span::raw(truncate_chars(&body, body_width)),
        ]),
        &pane.key.pane_id,
        Some(&pane.key.window.session.endpoint.socket),
    )
}

fn extend_inspector_roster(
    lines: &mut Vec<Line<'static>>,
    rows: Vec<Line<'static>>,
    total_rows: usize,
    area_height: u16,
    theme: WatchThemeSpec,
) {
    // Subtract the border and the aggregate status line inserted by the
    // caller after this body is built.
    let available =
        usize::from(area_height.saturating_sub(2)).saturating_sub(lines.len().saturating_add(1));
    if total_rows <= available {
        lines.extend(rows);
        return;
    }
    if available == 0 {
        return;
    }
    let visible = available.saturating_sub(1);
    let hidden = total_rows.saturating_sub(visible);
    lines.extend(rows.into_iter().take(visible));
    lines.push(Line::from(Span::styled(
        format!("+{hidden} more"),
        theme.dim_style(),
    )));
}

fn matching_window_capture<'a>(app: &'a App, window: &WindowNode) -> Option<&'a CapturedWindow> {
    app.window_capture
        .as_ref()
        .filter(|capture| capture.key == window.key && !capture.panes.is_empty())
}

fn window_mosaic_area(area: Rect, window: &WindowNode) -> Option<Rect> {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let has_attention = inspector_priority_agent(window.panes.iter())
        .is_some_and(|(_, agent)| agent_needs_attention(agent.state));
    // status + scope/load/room/cwd + optional attention + latest + rule.
    // The mosaic replaces the textual `panes` header and every roster row
    // below it, while those lines remain the no-cache fallback.
    let summary_height = 7_u16 + u16::from(has_attention);
    let mosaic = Rect::new(
        inner.x,
        inner.y.saturating_add(summary_height),
        inner.width,
        inner.height.saturating_sub(summary_height),
    );
    (mosaic.width >= WINDOW_MOSAIC_MIN_WIDTH && mosaic.height >= WINDOW_MOSAIC_MIN_HEIGHT)
        .then_some(mosaic)
}

pub(super) fn scaled_window_pane_rect(
    geometry: &PaneGeometry,
    source_width: u32,
    source_height: u32,
    area: Rect,
) -> Option<Rect> {
    if source_width == 0 || source_height == 0 || area.width == 0 || area.height == 0 {
        return None;
    }
    let scale = |position: u32, source: u32, target: u16| -> u16 {
        let value = position
            .saturating_mul(u32::from(target))
            .saturating_div(source)
            .min(u32::from(target));
        u16::try_from(value).unwrap_or(target)
    };
    let left = scale(u32::from(geometry.left), source_width, area.width);
    let top = scale(u32::from(geometry.top), source_height, area.height);
    let mut right = scale(
        u32::from(geometry.left) + u32::from(geometry.width),
        source_width,
        area.width,
    );
    let mut bottom = scale(
        u32::from(geometry.top) + u32::from(geometry.height),
        source_height,
        area.height,
    );
    right = right.max(left.saturating_add(1)).min(area.width);
    bottom = bottom.max(top.saturating_add(1)).min(area.height);
    if left >= area.width || top >= area.height || right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(
        area.x.saturating_add(left),
        area.y.saturating_add(top),
        right - left,
        bottom - top,
    ))
}

fn render_window_mosaic_cell(
    f: &mut Frame,
    rect: Rect,
    window: &WindowNode,
    captured: &CapturedWindowPane,
    theme: WatchThemeSpec,
    spin: Spinner,
) {
    let pane = window
        .panes
        .iter()
        .find(|pane| pane.key.pane_id == captured.geometry.pane_id);
    let agent = pane.and_then(|pane| pane.agent.as_ref());
    let (glyph, state_style) = agent.map_or_else(
        || ("○", theme.dim_style()),
        |agent| state_marker(agent.state, theme, spin),
    );
    let mut title = vec![
        Span::raw(" "),
        Span::styled(glyph.to_string(), state_style),
        Span::styled(
            format!(" {}", captured.geometry.pane_id),
            Style::default()
                .fg(crate::pane_identity::pane_color(
                    &captured.geometry.pane_id,
                    Some(&window.key.session.endpoint.socket),
                ))
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if rect.width >= 18 {
        let detail = agent.map_or_else(
            || captured.geometry.command.clone(),
            |agent| {
                format!(
                    " · {} {}",
                    agent_kind_short(agent.kind),
                    state_age_label(agent.state)
                )
            },
        );
        title.push(Span::styled(detail, theme.dim_style()));
    }
    title.push(Span::raw(" "));

    if rect.width < 5 || rect.height < 3 {
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(Line::from(title)), rect);
        return;
    }

    let border_style = agent.map_or_else(
        || theme.border_style(),
        |agent| {
            if agent_needs_attention(agent.state) || captured.geometry.active {
                theme.state_style(agent.state).add_modifier(Modifier::BOLD)
            } else {
                theme.border_style()
            }
        },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if captured.geometry.active {
            BorderType::Double
        } else {
            theme.border_type
        })
        .border_style(border_style)
        .title(Line::from(title));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(text) = captured.text.as_ref() else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "(capture unavailable)",
                theme.dim_style().add_modifier(Modifier::ITALIC),
            ))),
            inner,
        );
        return;
    };
    let scroll = text.lines.len().saturating_sub(usize::from(inner.height));
    f.render_widget(
        Paragraph::new(text.clone()).scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        inner,
    );
}

fn render_window_mosaic(
    f: &mut Frame,
    area: Rect,
    window: &WindowNode,
    capture: &CapturedWindow,
    theme: WatchThemeSpec,
    spin: Spinner,
) {
    f.render_widget(Clear, area);
    let header = Rect { height: 1, ..area };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("live layout", theme.accent_badge()),
            Span::styled("  pane geometry · 1s refresh", theme.dim_style()),
        ])),
        header,
    );
    let canvas = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    if canvas.height == 0 {
        return;
    }

    let source_width = capture
        .panes
        .iter()
        .map(|pane| u32::from(pane.geometry.left) + u32::from(pane.geometry.width))
        .max()
        .unwrap_or(0);
    let source_height = capture
        .panes
        .iter()
        .map(|pane| u32::from(pane.geometry.top) + u32::from(pane.geometry.height))
        .max()
        .unwrap_or(0);

    for captured in &capture.panes {
        let rect = if capture.zoomed && captured.geometry.active {
            Some(canvas)
        } else {
            scaled_window_pane_rect(&captured.geometry, source_width, source_height, canvas)
        };
        let Some(rect) = rect else {
            continue;
        };
        render_window_mosaic_cell(f, rect, window, captured, theme, spin);
    }
}

#[allow(clippy::too_many_lines)] // one hierarchy-specific inspector body per match arm
pub(super) fn render_topology_inspector(
    f: &mut Frame,
    area: Rect,
    app: &App,
    tree_targets: &[TreeTarget],
    theme: WatchThemeSpec,
) {
    let Some(node) = selected_tree_target_from(app, tree_targets)
        .and_then(|target| app.topology.find(&target.key))
    else {
        return;
    };
    let now = OffsetDateTime::now_utc();
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let spin = Spinner {
        frame: app.anim_frame,
        enabled: app.watch_cfg.spinner && icons_unicode(),
    };
    let (title, mut lines) = match node {
        TopologyNodeRef::Session(session) => {
            let attached = session
                .attached_clients
                .map_or_else(|| "unknown".into(), |count| count.to_string());
            let duration = unambiguous_session_activity_secs(app, session, now)
                .map_or_else(|| "—".into(), format_duration);
            let latest = inspector_latest_agent(
                session
                    .windows
                    .iter()
                    .flat_map(|window| window.panes.iter()),
            )
            .map_or_else(
                || "—".into(),
                |(pane, agent)| {
                    let window_name = session
                        .windows
                        .iter()
                        .find(|window| window.key == pane.key.window)
                        .map_or("?", |window| window.name.as_str());
                    format!(
                        "{} · {} · {window_name}/{} · {}",
                        relative_time(agent.last_activity_at, now),
                        agent_kind_short(agent.kind),
                        pane.key.pane_id,
                        inspector_agent_summary(agent)
                    )
                },
            );
            let mut session_lines = vec![
                inspector_field_line(
                    "scope",
                    format!(
                        "{} windows · {} panes · {} agents",
                        session.windows.len(),
                        session.pane_count(),
                        session.states.total()
                    ),
                    width,
                    theme,
                ),
                inspector_field_line(
                    "presence",
                    format!("{attached} clients · {duration} attached"),
                    width,
                    theme,
                ),
            ];
            if let Some((pane, agent)) = inspector_priority_agent(
                session
                    .windows
                    .iter()
                    .flat_map(|window| window.panes.iter()),
            ) {
                if agent_needs_attention(agent.state) {
                    let window_name = session
                        .windows
                        .iter()
                        .find(|window| window.key == pane.key.window)
                        .map_or("?", |window| window.name.as_str());
                    session_lines.push(inspector_attention_line(
                        format!("{window_name}/{}", pane.key.pane_id),
                        agent,
                        now,
                        width,
                        theme,
                    ));
                }
            }
            session_lines.extend([
                inspector_field_line("latest", latest, width, theme),
                inspector_field_line(
                    "identity",
                    format!(
                        "{}:{} · session {}",
                        session.key.endpoint.host,
                        session.key.endpoint.socket,
                        session.key.session_id
                    ),
                    width,
                    theme,
                ),
                inspector_rule(area.width, theme),
                Line::from(vec![
                    Span::styled("topology", theme.accent_badge()),
                    Span::styled(
                        if width >= 68 {
                            "  WINDOWS / PANES · A/P · STATE · AGE · MODEL/CTX · LATEST"
                        } else if width >= 56 {
                            "  WINDOWS / PANES · A/P · STATE · AGE · LATEST"
                        } else {
                            "  WINDOWS / PANES · STATE · LATEST"
                        },
                        theme.dim_style(),
                    ),
                ]),
            ]);
            let windows = app.sorted_windows(session.windows.iter());
            let total_rows = windows
                .iter()
                .map(|window| 1_usize.saturating_add(window.panes.len()))
                .sum::<usize>();
            let available = usize::from(area.height.saturating_sub(2))
                .saturating_sub(session_lines.len().saturating_add(1));
            let row_limit = if total_rows > available {
                available.saturating_sub(1)
            } else {
                available
            };
            let mut rows = Vec::with_capacity(row_limit);
            for window in windows {
                if rows.len() >= row_limit {
                    break;
                }
                rows.push(inspector_window_roster_line(
                    window, width, now, theme, spin,
                ));
                if rows.len() >= row_limit {
                    break;
                }
                let panes = app.sorted_panes(window.panes.iter());
                let pane_count = panes.len();
                for (index, pane) in panes.into_iter().enumerate() {
                    if rows.len() >= row_limit {
                        break;
                    }
                    let last = index + 1 == pane_count;
                    let branch = if icons_unicode() {
                        if last {
                            "  └─"
                        } else {
                            "  ├─"
                        }
                    } else if last {
                        "  `-"
                    } else {
                        "  +-"
                    };
                    rows.push(inspector_pane_roster_line_with_prefix(
                        pane, width, now, theme, spin, branch,
                    ));
                }
            }
            extend_inspector_roster(&mut session_lines, rows, total_rows, area.height, theme);
            (
                format!(" Inspector · session {} ", session.name),
                session_lines,
            )
        }
        TopologyNodeRef::Window(window) => {
            let mosaic_ready = matching_window_capture(app, window).is_some()
                && window_mosaic_area(area, window).is_some();
            let recent = inspector_latest_agent(window.panes.iter()).map_or_else(
                || "—".into(),
                |(pane, agent)| {
                    format!(
                        "{} · {} · {} · {}",
                        relative_time(agent.last_activity_at, now),
                        agent_kind_short(agent.kind),
                        pane.key.pane_id,
                        inspector_agent_summary(agent)
                    )
                },
            );
            let incoming = app
                .collaboration
                .incoming
                .iter()
                .filter(|request| {
                    participant_matches_window(&request.from, window)
                        || participant_matches_window(&request.to, window)
                })
                .count();
            let sent = app
                .collaboration
                .sent
                .iter()
                .filter(|request| {
                    participant_matches_window(&request.from, window)
                        || participant_matches_window(&request.to, window)
                })
                .count();
            let mailbox = app
                .collaboration
                .room
                .as_ref()
                .filter(|room| participant_matches_window(&room.current, window))
                .map_or_else(
                    || format!("not loaded · {incoming} incoming · {sent} sent"),
                    |room| {
                        // `unread` counts what was addressed to the sender.
                        // Nothing is ever addressed to a console, so printing
                        // it there is a permanent `0 unread` that reads as
                        // "no mail anywhere" — drop it and let the incoming
                        // and sent counts carry the line.
                        let unread = if room.current.console {
                            String::new()
                        } else {
                            format!(" · {} unread", room.unread)
                        };
                        format!(
                            "{} peers{unread} · {incoming} incoming · {sent} sent",
                            room.peers.len()
                        )
                    },
                );
            let load = inspector_load(&window.panes);
            let mut load_parts = vec![
                format!("{} processes", load.processes),
                format!("{} shells", load.shells),
                format!("{} subagents", load.subagents),
            ];
            if let Some(context) = load.max_context {
                load_parts.push(format!("ctx max {context:.0}%"));
            }
            if load.has_cost {
                load_parts.push(format!("cost ${:.2}", load.cost));
            }
            let mut window_lines = vec![
                inspector_field_line(
                    "scope",
                    format!(
                        "{} panes · {} agents",
                        window.panes.len(),
                        window.states.total()
                    ),
                    width,
                    theme,
                ),
                inspector_field_line("load", load_parts.join(" · "), width, theme),
                inspector_field_line(
                    "room",
                    format!(
                        "{}:{}:{} · {mailbox}",
                        window.key.session.endpoint.host,
                        window.key.session.endpoint.socket,
                        window.key.window_id
                    ),
                    width,
                    theme,
                ),
                inspector_field_line(
                    "cwd",
                    window.cwd.as_deref().unwrap_or("mixed/unknown").into(),
                    width,
                    theme,
                ),
            ];
            if let Some((pane, agent)) = inspector_priority_agent(window.panes.iter()) {
                if agent_needs_attention(agent.state) {
                    window_lines.push(inspector_attention_line(
                        pane.key.pane_id.clone(),
                        agent,
                        now,
                        width,
                        theme,
                    ));
                }
            }
            window_lines.extend([
                inspector_field_line("latest", recent, width, theme),
                inspector_rule(area.width, theme),
            ]);
            if !mosaic_ready {
                window_lines.push(Line::from(vec![
                    Span::styled("panes", theme.accent_badge()),
                    Span::styled(
                        if width >= 68 {
                            "  AGENT    STATE  AGE  MODEL      CTX  LATEST"
                        } else {
                            "  AGENT    STATE  AGE  LATEST"
                        },
                        theme.dim_style(),
                    ),
                ]));
                let panes = app.sorted_panes(window.panes.iter());
                let rows = panes
                    .into_iter()
                    .map(|pane| inspector_pane_roster_line(pane, width, now, theme, spin))
                    .collect::<Vec<_>>();
                let total_rows = rows.len();
                extend_inspector_roster(&mut window_lines, rows, total_rows, area.height, theme);
            }
            (
                format!(" Inspector · window {} ", window.name),
                window_lines,
            )
        }
        TopologyNodeRef::Pane(pane) => {
            let mut pane_lines = vec![
                Line::from(format!(
                    "pane  {}  ·  pid {}  ·  {}",
                    pane.key.pane_id, pane.pane_pid, pane.current_command
                )),
                Line::from(format!(
                    "endpoint  {}:{}  ·  session {}  ·  window {}",
                    pane.key.window.session.endpoint.host,
                    pane.key.window.session.endpoint.socket,
                    pane.key.window.session.session_id,
                    pane.key.window.window_id
                )),
                Line::from(format!("cwd  {}", pane.cwd)),
            ];
            if let Some(agent) = pane.agent.as_ref() {
                pane_lines.extend([
                    Line::from(format!(
                        "agent  {}  ·  {} {}",
                        agent_kind_short(agent.kind),
                        state_age_label(agent.state),
                        relative_time(agent.state_entered_at, now)
                    )),
                    Line::from(format!(
                        "model  {}  ·  ctx {}  ·  cost {}",
                        agent.model.as_deref().unwrap_or("—"),
                        agent
                            .context_used_pct
                            .map_or_else(|| "—".into(), |value| format!("{value:.0}%")),
                        agent
                            .cost_usd
                            .map_or_else(|| "—".into(), |value| format!("${value:.2}"))
                    )),
                    Line::from(format!(
                        "processes  {}  ·  shells {}  ·  subagents {}",
                        agent.workload.process_count,
                        agent.workload.shell_count,
                        agent.workload.subagent_count
                    )),
                    Line::from(format!(
                        "prompt  {}",
                        truncate_chars(
                            &agent
                                .last_prompt
                                .as_deref()
                                .unwrap_or("—")
                                .replace('\n', " "),
                            usize::from(area.width.saturating_sub(10))
                        )
                    )),
                    Line::from(format!(
                        "response  {}",
                        truncate_chars(
                            &agent
                                .last_response
                                .as_deref()
                                .unwrap_or("—")
                                .replace('\n', " "),
                            usize::from(area.width.saturating_sub(12))
                        )
                    )),
                ]);
                if !agent.workload.preview.is_empty() {
                    pane_lines.push(Line::from(Span::styled(
                        "process tree",
                        theme.accent_badge(),
                    )));
                    pane_lines.extend(agent.workload.preview.iter().map(|process| {
                        let branch = "  ".repeat(usize::from(process.depth.saturating_sub(1)));
                        Line::from(format!(
                            "{branch}{}  pid {}  {}",
                            match process.kind {
                                WorkloadProcessKind::Subagent => "◇",
                                WorkloadProcessKind::Shell => "▸",
                                WorkloadProcessKind::Helper => "·",
                                WorkloadProcessKind::Process => "+",
                            },
                            process.pid,
                            process.command
                        ))
                    }));
                }
            }
            pane_lines.push(inspector_rule(area.width, theme));
            pane_lines.extend(
                build_pane_capture_body_on(
                    app,
                    &pane.key.pane_id,
                    Some(&pane.key.window.session.endpoint.socket),
                )
                .lines,
            );
            (
                format!(" Inspector · pane {} ", pane.key.pane_id),
                pane_lines,
            )
        }
    };
    lines.insert(0, topology_inspector_status_line(node, app, theme));
    let title = match node {
        TopologyNodeRef::Pane(pane) => crate::pane_identity::address_line(
            title,
            &pane.key.pane_id,
            Some(&pane.key.window.session.endpoint.socket),
        ),
        _ => Line::raw(title),
    }
    .style(theme.accent_badge());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(title);
    f.render_widget(Paragraph::new(lines).block(block), area);
    if let TopologyNodeRef::Window(window) = node {
        if let (Some(capture), Some(mosaic_area)) = (
            matching_window_capture(app, window),
            window_mosaic_area(area, window),
        ) {
            render_window_mosaic(f, mosaic_area, window, capture, theme, spin);
        }
    }
}
