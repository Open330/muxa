use super::{
    agent_pane_display, format_duration, icons_unicode, plural, prepend_host_badge,
    prepend_tree_prefix, pulse_cell, resolve_row_pulses, resolve_width, row_host, rows_multi_host,
    watch_theme, Agent, Alignment, App, Block, Borders, Cell, Color, Constraint, Frame,
    HighlightSpacing, Line, Modifier, OffsetDateTime, PaneInfo, Paragraph, PulseKind,
    RateLimitScope, Rect, Row, Span, Spinner, Style, Table, Text, WatchColumn, WatchRow,
    WatchSummary, WatchThemeSpec, WatchView, WorkRow, WorkloadProcessKind, SUMMARY_MIN_TABLE_WIDTH,
};

/// The columns the table actually renders at `table_width`. Only SUMMARY
/// folds: DUR and ACT are six columns each and part of the row identity,
/// so there is no width at which dropping them beats keeping them.
pub(super) fn effective_columns(columns: &[WatchColumn], table_width: u16) -> Vec<WatchColumn> {
    if table_width >= SUMMARY_MIN_TABLE_WIDTH || columns.len() <= 1 {
        return columns.to_vec();
    }
    let trimmed: Vec<WatchColumn> = columns
        .iter()
        .copied()
        .filter(|c| !matches!(c, WatchColumn::Prompt))
        .collect();
    if trimmed.is_empty() {
        columns.to_vec()
    } else {
        trimmed
    }
}

#[allow(clippy::too_many_lines)] // column resolution + per-row cell/badge/pulse
                                 // assembly reads better inline than split across helpers
pub(super) fn render_table(f: &mut Frame, area: Rect, app: &mut App) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let columns = effective_columns(&app.columns, area.width);
    let visible_targets = app.visible_targets();

    // Empty grid reads as "muxa is broken" rather than "nothing is
    // running yet". Replace it with a centered hint that tells the user
    // what to do — and, when rows exist but are all hidden paneless
    // agents, says so instead of implying there's nothing at all.
    if visible_targets.is_empty() {
        render_empty_table(f, area, app, theme);
        return;
    }

    let header_cells = columns.iter().map(|c| {
        let header = if app.watch_cfg.view == WatchView::Window && matches!(c, WatchColumn::Pane) {
            "WORKSPACE › WORK"
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
    let detail_host = detail_host_column(&columns);
    let status_host = status_host_column(&columns);
    let spin = Spinner {
        frame: app.anim_frame,
        enabled: app.watch_cfg.spinner && icons_unicode(),
    };
    // A one-shot done/error flash overrides the State cell for its window.
    // Resolve per-row pulses up front so the row closure only touches app
    // *fields* — calling an `&self` method inside it would capture all of
    // `app` and clash with the `&mut app.table_state` render below.
    let anim_frame = app.anim_frame;
    let state_col = columns
        .iter()
        .position(|c| matches!(c, WatchColumn::State | WatchColumn::StateAge));
    // Host badges: only when the row set spans >1 host (the cross-
    // multiplexer console) do we tag each row's SESSION/PANE cell, so a
    // single-host user sees no change. The Pane column is the natural
    // host-identifying slot in both the session and pane views.
    let pane_col = columns.iter().position(|c| matches!(c, WatchColumn::Pane));
    let multi_host = rows_multi_host(&app.rows);
    let row_pulses = resolve_row_pulses(app);
    let target_pulses: Vec<Option<PulseKind>> = visible_targets
        .iter()
        .map(|target| {
            target
                .agent_idx
                .map_or(row_pulses[target.row_idx], |agent_idx| {
                    let WatchRow::Work(session) = &app.rows[target.row_idx] else {
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
                (WatchRow::Work(session), Some(agent_idx)) => session.agents.get(agent_idx),
                _ => None,
            };
            let mut texts: Vec<Text> = if let Some(agent) = child_agent {
                columns
                    .iter()
                    .map(|c| {
                        c.agent_text(agent, now, &app.panes, theme, spin, app.watch_cfg.summary)
                    })
                    .collect()
            } else {
                match r {
                    WatchRow::Agent(a) => columns
                        .iter()
                        .map(|c| {
                            c.agent_text(a, now, &app.panes, theme, spin, app.watch_cfg.summary)
                        })
                        .collect(),
                    WatchRow::BarePane(p) => columns.iter().map(|c| c.bare_text(p)).collect(),
                    WatchRow::Work(s) => columns
                        .iter()
                        .map(|c| {
                            c.work_text(s, now, &app.panes, theme, spin, app.watch_cfg.summary)
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
                } else if matches!(r, WatchRow::Work(_)) {
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
                // work rows continue to use their latest-agent fallback.
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

    // When SUMMARY folded away, let the name column grow into the slack —
    // truncated names were the whole reason to fold — but with a ceiling:
    // a name column that swallows every freed column just trades an
    // unreadable summary for a field of trailing blanks. 30 covers the
    // 8-cell status gutter plus a 22-character session name.
    let summary_folded = columns.len() != app.columns.len();
    let widths: Vec<Constraint> = columns
        .iter()
        .map(|c| {
            if summary_folded && matches!(c, WatchColumn::Pane) {
                Constraint::Max(30)
            } else {
                resolve_width(*c, &app.watch_cfg)
            }
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style())
                .border_type(theme.border_type)
                .title(if app.watch_cfg.view == WatchView::Window {
                    " Works "
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
pub(super) fn onboarding_empty(app: &App) -> bool {
    app.rows.is_empty()
        && app.search_query.is_empty()
        && !app.attention_only
        && app.paneless_hidden == 0
        && app.paneless_attention == 0
        && app.last_error.is_none()
}

pub(super) fn render_empty_table(f: &mut Frame, area: Rect, app: &App, theme: WatchThemeSpec) {
    let title = if app.uses_tree() {
        " Topology "
    } else if app.watch_cfg.view == WatchView::Window {
        " Windows "
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
                "No work or agents match ‘{}’.",
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
            "Start your first coding agent",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Enter / n: start an agent in the current folder",
            theme.dim_style(),
        )));
    }

    if onboarding_empty(app) {
        if app.demo_available {
            lines.push(Line::from(Span::styled(
                "d: try the three-step demo · no API calls",
                theme.dim_style(),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Already running one? r: refresh · muxa doctor: check detection",
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
pub(super) fn detail_host_column(cols: &[WatchColumn]) -> Option<usize> {
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

pub(super) fn truncate_chars(s: &str, max: usize) -> String {
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
pub(super) fn format_detail(
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
            "pane" => agent_pane_display(a, panes),
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
        WatchRow::Work(s) => {
            if let Some(a) = s.latest_agent.as_ref() {
                return resolve_var(name, &WatchRow::agent(a.clone()), panes, now);
            }
            Some(match name {
                "pane" => s.session.clone(),
                "kind" => "work".into(),
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
/// default `Recap` falls through response → recap → session title → last prompt. That
/// matters because recaps are sparse (Claude Code writes one only when you
/// return after being away; Codex prints one only after context compaction)
/// and agents with no recap source, such as Gemini, would otherwise render an
/// empty column.
pub(super) fn summary_line(a: &Agent, mode: WatchSummary) -> String {
    let picked = match mode {
        WatchSummary::Recap => a.summary_text().or(a.last_prompt.as_deref()),
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

pub(super) fn workload_text(a: &Agent) -> Text<'static> {
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

pub(super) fn work_workload_text(s: &WorkRow) -> Text<'static> {
    let label = work_workload_badge(s).unwrap_or_else(|| "-".into());
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
        WatchRow::Work(s) => work_workload_badge(s),
        WatchRow::BarePane(_) => None,
    }
}

pub(super) fn agent_workload_badge(a: &Agent) -> Option<String> {
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

fn work_workload_badge(s: &WorkRow) -> Option<String> {
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

pub(super) fn workload_detail_string(a: &Agent) -> String {
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
pub(super) fn limits_text(a: &Agent, now: OffsetDateTime) -> Text<'static> {
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

pub(super) fn relative_time(at: OffsetDateTime, now: OffsetDateTime) -> String {
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
