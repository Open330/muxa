use super::{
    agent_workload_badge, format_duration, plural, pulse_glyph_style, relative_time,
    render_empty_table, state_summary_gutter_spans, truncate_chars,
    unambiguous_session_activity_secs, watch_theme, Agent, AgentKind, AgentState, App, Block,
    Borders, Cell, Constraint, Frame, HashMap, HashSet, HighlightSpacing, IconSet, Line, Modifier,
    OffsetDateTime, Paragraph, PulseKind, Rect, Row, Span, Spinner, Style, Table, Text,
    TopologyNodeRef, TreeTarget, WatchLayout, WatchRow, WatchThemeSpec, WindowNode, WorkRow,
};

pub(super) fn pipeline_status_label(run: &tend::pipeline_run::PipelineRunSummary) -> String {
    let aliases = run
        .aliases
        .iter()
        .map(|alias| format!("{}:{}", alias.alias, alias.status))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} g{} {aliases}", run.pipeline, run.generation)
}

// cli-spinners frames: `dots` for parent agents, `dots2` (denser) for
// subagents, and a half-circle set for starting.
pub(super) const SWARM_DOTS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SWARM_DOTS2: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
pub(super) const SWARM_START: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// Whether the active `[ui] icons` set can render the braille/half-circle
/// spinner. Under `icons = "ascii"` the spinner is suppressed so a terminal
/// that can't draw unicode doesn't get mojibake — it falls back to the static
/// ascii `state_icon`.
pub(super) fn icons_unicode() -> bool {
    matches!(crate::icon_set(), IconSet::Unicode)
}

fn swarm_glyph(state: AgentState, frame: usize) -> &'static str {
    if !icons_unicode() {
        return crate::state_icon(state);
    }
    match state {
        AgentState::Working => SWARM_DOTS[frame % SWARM_DOTS.len()],
        AgentState::Starting => SWARM_START[frame % SWARM_START.len()],
        other => crate::state_icon(other),
    }
}

fn subagent_glyph(frame: usize, phase: usize) -> &'static str {
    if !icons_unicode() {
        return crate::state_icon(AgentState::Working);
    }
    SWARM_DOTS2[frame.wrapping_add(phase) % SWARM_DOTS2.len()]
}

/// Short, fixed vocabulary for an agent kind — `AgentKind`'s own
/// `Display` is the wire form (`claude_code`), which is too wide for a
/// swarm row or a `tend peek` box header.
pub(crate) fn agent_kind_short(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Antigravity => "agy",
        AgentKind::Opencode => "opencode",
        AgentKind::Copilot => "copilot",
        AgentKind::Task => "task",
        AgentKind::Unknown => "agent",
    }
}

fn swarm_state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Starting => "starting",
        AgentState::WaitingInput => "waiting·in",
        AgentState::WaitingChoice => "waiting·y/n",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
    }
}

/// `◇subagents ▸shells +other` load badge. Prefers the hook-tracked named
/// subagent count over the process-tree scan.
fn swarm_load(agent: &Agent) -> String {
    if !agent.subagents.is_empty() {
        return format!("◇{}", agent.subagents.len());
    }
    let w = &agent.workload;
    let mut parts = Vec::new();
    if w.subagent_count > 0 {
        parts.push(format!("◇{}", w.subagent_count));
    }
    if w.shell_count > 0 {
        parts.push(format!("▸{}", w.shell_count));
    }
    let other = w
        .process_count
        .saturating_sub(w.subagent_count)
        .saturating_sub(w.shell_count);
    if other > 0 {
        parts.push(format!("+{other}"));
    }
    parts.join(" ")
}

/// One cluster header line: `▐ workspace › work [n]  <state spinners>  n⬆ n⊂`.
fn swarm_cluster_header(
    sr: &WorkRow,
    theme: WatchThemeSpec,
    frame: usize,
    is_sel: bool,
) -> Line<'static> {
    let working = sr
        .agents
        .iter()
        .filter(|a| a.state == AgentState::Working)
        .count();
    let subs: usize = sr.agents.iter().map(|a| a.subagents.len()).sum();
    let mut hdr = vec![
        Span::styled(
            if is_sel { "▐ " } else { "  " },
            Style::default().fg(theme.accent),
        ),
        Span::styled(
            sr.display_name.clone(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  [{}]  ", sr.agents.len().max(sr.pane_count)),
            theme.dim_style(),
        ),
    ];
    for a in &sr.agents {
        hdr.push(Span::styled(
            format!("{} ", swarm_glyph(a.state, frame)),
            theme.state_style(a.state),
        ));
    }
    if working > 0 || subs > 0 {
        hdr.push(Span::styled(
            format!(" {working}⬆ {subs}⊂"),
            theme.dim_style(),
        ));
    }
    Line::from(hdr)
}

/// One agent row plus its indented subagent tree (`├─ / └─` with a dot
/// spinner per in-flight Task child).
fn swarm_agent_lines(
    agent: &Agent,
    theme: WatchThemeSpec,
    frame: usize,
    pulse: Option<PulseKind>,
) -> Vec<Line<'static>> {
    // A live transition pulse takes over the leading glyph; otherwise the
    // spinner/static state glyph.
    let (glyph, gstyle) = match pulse {
        Some(kind) => pulse_glyph_style(kind, theme, frame),
        None => (
            swarm_glyph(agent.state, frame).to_string(),
            theme.state_style(agent.state),
        ),
    };
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(format!("{glyph} "), gstyle),
        Span::styled(
            format!("{:<9}", agent_kind_short(agent.kind)),
            Style::default(),
        ),
        Span::styled(
            format!("{:<12}", swarm_state_label(agent.state)),
            theme.state_style(agent.state),
        ),
    ];
    let load = swarm_load(agent);
    if !load.is_empty() {
        spans.push(Span::styled(
            format!("{load:<7}"),
            Style::default().fg(theme.state_working),
        ));
    }
    if let Some(p) = agent.last_prompt.as_deref() {
        let snippet = p.replace('\n', " ");
        spans.push(Span::styled(
            truncate_chars(&snippet, 42),
            theme.dim_style(),
        ));
    }
    let mut out = vec![Line::from(spans)];

    let n = agent.subagents.len();
    for (j, s) in agent.subagents.iter().enumerate() {
        let conn = if j + 1 == n { "└─" } else { "├─" };
        let mut ss = vec![
            Span::raw("       "),
            Span::styled(format!("{conn} "), theme.dim_style()),
            Span::styled(
                format!("{} ", subagent_glyph(frame, j)),
                Style::default().fg(theme.state_working),
            ),
            Span::styled(
                format!("{:<16}", truncate_chars(&s.kind, 16)),
                Style::default(),
            ),
        ];
        if let Some(d) = s.description.as_deref() {
            ss.push(Span::styled(truncate_chars(d, 32), theme.dim_style()));
        }
        out.push(Line::from(ss));
    }
    out
}

/// The swarm console: one cluster per tmux work window, animated dot spinners
/// for working/starting agents, and an indented subagent tree under each
/// agent. Selection (j/k) highlights the work cluster.
pub(super) fn render_swarm(f: &mut Frame, area: Rect, app: &mut App, tree_targets: &[TreeTarget]) {
    if app.uses_tree() {
        render_topology_table(f, area, app, tree_targets, " Swarm topology ");
        return;
    }
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    let frame = app.anim_frame;
    let selected = app.table_state.selected();
    let now_pulse = std::time::Instant::now();
    let mut lines: Vec<Line> = Vec::new();
    let mut sel_line: Option<usize> = None;

    let visible_targets = app.visible_targets();
    for (i, target) in visible_targets.iter().enumerate() {
        let row = &app.rows[target.row_idx];
        let WatchRow::Work(sr) = row else {
            continue;
        };
        let is_sel = selected == Some(i);
        if is_sel {
            sel_line = Some(lines.len());
        }
        lines.push(swarm_cluster_header(sr, theme, frame, is_sel));
        for a in &sr.agents {
            let pulse = app.active_pulse(a.kind, &a.session_id, now_pulse);
            lines.extend(swarm_agent_lines(a, theme, frame, pulse));
        }
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no agents — waiting for a hook or discovery scan",
            theme.dim_style(),
        )));
    }

    // Keep the selected cluster on screen without a full scrollbar widget.
    let inner_h = usize::from(area.height.saturating_sub(2));
    let total = lines.len();
    let scroll_lines = if total <= inner_h {
        0
    } else {
        sel_line
            .unwrap_or(0)
            .saturating_sub(inner_h / 3)
            .min(total - inner_h)
    };
    let scroll = u16::try_from(scroll_lines).unwrap_or(u16::MAX);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .border_type(theme.border_type)
        .title(Span::styled(" Swarm ", theme.table_header_style()));
    let para = Paragraph::new(lines).block(block).scroll((scroll, 0));
    f.render_widget(para, area);
}

pub(super) fn tree_node_states(node: TopologyNodeRef<'_>) -> Vec<AgentState> {
    match node {
        TopologyNodeRef::Session(session) => session
            .windows
            .iter()
            .flat_map(|window| &window.panes)
            .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.state))
            .collect(),
        TopologyNodeRef::Window(window) => window
            .panes
            .iter()
            .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.state))
            .collect(),
        TopologyNodeRef::Pane(pane) => pane
            .agent
            .as_ref()
            .map(|agent| vec![agent.state])
            .unwrap_or_default(),
    }
}

fn target_window<'a>(target: &TreeTarget, node: TopologyNodeRef<'a>) -> Option<&'a WindowNode> {
    match node {
        TopologyNodeRef::Session(session) => target
            .compacted_window
            .as_ref()
            .and_then(|key| session.windows.iter().find(|window| &window.key == key)),
        TopologyNodeRef::Window(window) => Some(window),
        TopologyNodeRef::Pane(_) => None,
    }
}

pub(super) fn tree_state_cell(
    target: &TreeTarget,
    node: TopologyNodeRef<'_>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Text<'static> {
    // An expanded parent with exactly one child rolls up the exact same state
    // set as the row immediately below it. Leaving both visible turns the
    // common session -> window -> pane shape into three identical markers.
    // Keep the hierarchy selectable, but let the deepest visible owner carry
    // the status. Collapsed parents and real branch points still need their
    // aggregate because it adds information not otherwise visible.
    if tree_state_is_redundant(target, node) {
        return Text::default();
    }
    let states = tree_node_states(node);
    if states.is_empty() {
        return Text::from(Span::styled("○", theme.dim_style()));
    }
    Text::from(Line::from(state_summary_gutter_spans(states, theme, spin)))
}

pub(super) fn tree_state_is_redundant(target: &TreeTarget, node: TopologyNodeRef<'_>) -> bool {
    if !target.expanded {
        return false;
    }
    match node {
        TopologyNodeRef::Session(session) => target_window(target, node).map_or_else(
            || session.windows.len() == 1,
            |window| window.panes.len() == 1,
        ),
        TopologyNodeRef::Window(window) => window.panes.len() == 1,
        TopologyNodeRef::Pane(_) => false,
    }
}

/// What to call a pane in the tree, after its id.
///
/// The pipeline alias wins when there is one. `claude` and `codex` name the
/// CLI, which stops distinguishing anything the moment one work runs two of
/// the same kind — and a pair whose reviewer and implementer are both claude
/// then reads as two identical rows. `impl` and `review` say what the pane is
/// *for*. The CLI stays visible on the row (state glyph, model column) and in
/// the inspector, so nothing is lost.
pub(super) fn pane_label_owner(pane: &tend::topology::PaneNode) -> String {
    pane.agent_alias.clone().unwrap_or_else(|| {
        pane.agent.as_ref().map_or_else(
            || pane.current_command.clone(),
            |agent| agent_kind_short(agent.kind).to_string(),
        )
    })
}

pub(super) fn tree_node_label(
    target: &TreeTarget,
    node: TopologyNodeRef<'_>,
    show_endpoint: bool,
    theme: WatchThemeSpec,
) -> Text<'static> {
    let branch = match target.depth {
        0 => "",
        1 => {
            if target.is_last_sibling {
                "  └─ "
            } else {
                "  ├─ "
            }
        }
        _ => match (target.parent_is_last_sibling, target.is_last_sibling) {
            (true, true) => "    └─ ",
            (true, false) => "    ├─ ",
            (false, true) => "  │ └─ ",
            (false, false) => "  │ ├─ ",
        },
    };
    let disclosure = if target.has_children {
        if target.expanded {
            "▼ "
        } else {
            "▸ "
        }
    } else {
        "  "
    };
    let mut spans = vec![Span::styled(branch.to_string(), theme.dim_style())];
    if target.depth == 0 && show_endpoint {
        let TopologyNodeRef::Session(session) = node else {
            unreachable!("depth-zero tree nodes are sessions")
        };
        spans.push(Span::styled(
            format!(
                "{}:{} ",
                session.key.endpoint.host, session.key.endpoint.socket
            ),
            theme.dim_style(),
        ));
    }
    spans.push(Span::styled(disclosure.to_string(), theme.dim_style()));
    let label = match node {
        TopologyNodeRef::Session(session) => target_window(target, node).map_or_else(
            || session.name.clone(),
            |window| format!("{} › {}", session.name, window.name),
        ),
        TopologyNodeRef::Window(window) => window.name.clone(),
        TopologyNodeRef::Pane(pane) => {
            let owner = pane_label_owner(pane);
            if owner.trim().is_empty() {
                pane.key.pane_id.clone()
            } else {
                format!("{}  {owner}", pane.key.pane_id)
            }
        }
    };
    if let TopologyNodeRef::Pane(pane) = node {
        spans.extend(
            crate::pane_identity::address_line(
                label,
                &pane.key.pane_id,
                Some(&pane.key.window.session.endpoint.socket),
            )
            .spans,
        );
    } else if target.depth == 0 {
        spans.push(Span::styled(
            label,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw(label));
    }
    // A work window that has finished looks exactly like one that stalled —
    // every agent idle — unless the row says so.
    if let Some(window) = target_window(target, node) {
        if let Some(completion) = window.completion {
            let style = if completion.is_complete() {
                Style::default().fg(theme.state_idle)
            } else {
                theme.dim_style()
            };
            spans.push(Span::styled(
                format!("  {}/{}", completion.done, completion.total),
                style,
            ));
        }
        if let Some(run) = window.pipeline_run.as_ref() {
            spans.push(Span::styled(
                format!("  {}", pipeline_status_label(run)),
                theme.dim_style(),
            ));
        }
    }
    Text::from(Line::from(spans))
}

fn window_node_age(window: &WindowNode, now: OffsetDateTime) -> String {
    window
        .panes
        .iter()
        .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.started_at))
        .min()
        .map_or_else(
            || "—".into(),
            |started| {
                format_duration(
                    u64::try_from((now - started).whole_seconds().max(0)).unwrap_or(u64::MAX),
                )
            },
        )
}

fn tree_node_age(
    app: &App,
    target: &TreeTarget,
    node: TopologyNodeRef<'_>,
    now: OffsetDateTime,
) -> String {
    if let Some(window) = target_window(target, node) {
        return window_node_age(window, now);
    }
    match node {
        TopologyNodeRef::Session(session) => unambiguous_session_activity_secs(app, session, now)
            .map_or_else(|| "—".into(), format_duration),
        TopologyNodeRef::Window(window) => window_node_age(window, now),
        TopologyNodeRef::Pane(pane) => pane.agent.as_ref().map_or_else(
            || "—".into(),
            |agent| relative_time(agent.state_entered_at, now),
        ),
    }
}

const WINDOW_ACTIVITY_SUMMARY_LIMIT: usize = 2;
const WINDOW_ACTIVITY_ITEM_CHARS: usize = 42;

fn topology_agent_summary(agent: &Agent, layout: WatchLayout) -> String {
    let raw = if layout == WatchLayout::Swarm {
        swarm_topology_agent_summary(agent)
    } else {
        agent
            .last_notification
            .as_deref()
            .or(agent.summary_text())
            .or(agent.last_prompt.as_deref())
            .map_or_else(
                || agent.state.to_string(),
                |text| tend::prompt_headline(text).into_owned(),
            )
    };
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&one_line, WINDOW_ACTIVITY_ITEM_CHARS)
}

pub(super) fn window_node_summary(window: &WindowNode, layout: WatchLayout) -> String {
    let mut agents = window
        .panes
        .iter()
        .filter_map(|pane| {
            pane.agent.as_ref().map(|agent| {
                (
                    agent.last_activity_at,
                    pane.key.pane_id.as_str(),
                    topology_agent_summary(agent, layout),
                )
            })
        })
        .collect::<Vec<_>>();
    if agents.is_empty() {
        return format!(
            "{} pane{} · no tracked agents",
            window.panes.len(),
            plural(window.panes.len())
        );
    }
    agents.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));

    // Preserve recent-first order while folding identical child activity.
    // This makes a window row describe the whole room instead of copying its
    // currently active pane, without wasting space when several agents share
    // the same prompt/title.
    let mut grouped: Vec<(String, usize)> = Vec::new();
    for (_, _, summary) in &agents {
        if let Some((_, count)) = grouped.iter_mut().find(|(seen, _)| seen == summary) {
            *count += 1;
        } else {
            grouped.push((summary.clone(), 1));
        }
    }

    let mut parts = grouped
        .iter()
        .take(WINDOW_ACTIVITY_SUMMARY_LIMIT)
        .map(|(summary, count)| {
            if *count > 1 {
                format!("{summary} ×{count}")
            } else {
                summary.clone()
            }
        })
        .collect::<Vec<_>>();
    let hidden_agents = grouped
        .iter()
        .skip(WINDOW_ACTIVITY_SUMMARY_LIMIT)
        .map(|(_, count)| count)
        .sum::<usize>();
    if hidden_agents > 0 {
        parts.push(format!("+{hidden_agents}"));
    }
    let aggregate = parts.join(" / ");
    if agents.len() == 1 {
        aggregate
    } else {
        format!("{} agents · {aggregate}", agents.len())
    }
}

fn tree_node_summary(
    target: &TreeTarget,
    node: TopologyNodeRef<'_>,
    layout: WatchLayout,
) -> String {
    if let Some(window) = target_window(target, node) {
        return window_node_summary(window, layout);
    }
    match node {
        TopologyNodeRef::Session(session) => {
            let agents = session.states.total();
            format!(
                "{} window{} · {agents} agent{}",
                session.windows.len(),
                plural(session.windows.len()),
                plural(agents)
            )
        }
        TopologyNodeRef::Window(window) => window_node_summary(window, layout),
        TopologyNodeRef::Pane(pane) => pane.agent.as_ref().map_or_else(
            || {
                if pane.title.is_empty() || pane.title == pane.current_command {
                    pane.current_command.clone()
                } else {
                    format!("{} · {}", pane.current_command, pane.title)
                }
            },
            |agent| topology_agent_summary(agent, layout),
        ),
    }
}

fn swarm_topology_agent_summary(agent: &Agent) -> String {
    let mut parts = Vec::new();
    if let Some(badge) = agent_workload_badge(agent) {
        parts.push(badge);
    }
    if !agent.subagents.is_empty() {
        if parts.iter().all(|part| !part.contains('◇')) {
            parts.push(format!("◇{}", agent.subagents.len()));
        }
        parts.push(
            agent
                .subagents
                .iter()
                .map(|subagent| subagent.kind.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    let activity = agent
        .last_notification
        .as_deref()
        .or(agent.summary_text())
        .or(agent.last_prompt.as_deref())
        .map_or(std::borrow::Cow::Borrowed("waiting"), tend::prompt_headline)
        .replace('\n', " ");
    parts.push(activity);
    parts.join(" · ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkTableColumn {
    State,
    Work,
    Workspace,
    Generation,
    Aliases,
    Done,
    Cwd,
}

impl WorkTableColumn {
    fn header(self) -> &'static str {
        match self {
            Self::State => "STATE",
            Self::Work => "WORK",
            Self::Workspace => "WORKSPACE",
            Self::Generation => "GEN",
            Self::Aliases => "ALIASES",
            Self::Done => "DONE",
            Self::Cwd => "CWD",
        }
    }

    fn constraint(self) -> Constraint {
        match self {
            Self::State => Constraint::Length(7),
            Self::Work | Self::Cwd => Constraint::Min(16),
            Self::Workspace => Constraint::Length(16),
            Self::Generation | Self::Done => Constraint::Length(5),
            Self::Aliases => Constraint::Min(18),
        }
    }
}

/// Columns that fit `width`, dropped in reverse order of what identifies a
/// Work. WORK and its live state survive to the narrowest popup; the cwd and
/// the workspace are the first to go, being the most inferable from the rest.
fn work_table_columns(width: u16) -> Vec<WorkTableColumn> {
    let mut columns = vec![
        WorkTableColumn::State,
        WorkTableColumn::Work,
        WorkTableColumn::Aliases,
        WorkTableColumn::Done,
    ];
    if width >= 80 {
        columns.insert(2, WorkTableColumn::Workspace);
    }
    if width >= 96 {
        columns.insert(columns.len() - 1, WorkTableColumn::Generation);
    }
    if width >= 110 {
        columns.push(WorkTableColumn::Cwd);
    }
    columns
}

/// Who is in the Work: pipeline aliases with their statuses when it has a Run,
/// otherwise the agent programs collapsed so three claudes read as `claude x3`
/// rather than three identical words.
fn work_alias_summary(window: &tend::topology::WindowNode) -> String {
    if let Some(run) = window.pipeline_run.as_ref() {
        if !run.aliases.is_empty() {
            return run
                .aliases
                .iter()
                .map(|alias| format!("{}:{}", alias.alias, alias.status))
                .collect::<Vec<_>>()
                .join(" · ");
        }
    }
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for pane in &window.panes {
        let Some(agent) = pane.agent.as_ref() else {
            continue;
        };
        let label = agent_kind_short(agent.kind);
        match counts.iter_mut().find(|(name, _)| *name == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
    }
    if counts.is_empty() {
        return "-".to_string();
    }
    counts
        .into_iter()
        .map(|(name, count)| {
            if count == 1 {
                name.to_string()
            } else {
                format!("{name} x{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `done/total`, or `-` when nothing here was ever asked to report.
///
/// A hand-split window has no pipeline, and `0/0` there would read as work
/// that has not finished rather than work that never reported.
fn work_done_cell(window: &tend::topology::WindowNode) -> String {
    match window.completion {
        None => "-".to_string(),
        Some(completion) if completion.total == 0 => "-".to_string(),
        Some(completion) => format!("{}/{}", completion.done, completion.total),
    }
}

fn work_cwd_cell(window: &tend::topology::WindowNode) -> String {
    let Some(cwd) = window.cwd.as_deref() else {
        return "-".to_string();
    };
    let Some(home) = dirs::home_dir() else {
        return cwd.to_string();
    };
    std::path::Path::new(cwd)
        .strip_prefix(&home)
        .map_or_else(|_| cwd.to_string(), |rest| format!("~/{}", rest.display()))
}

/// The flat one-row-per-Work table.
///
/// Mirrors `tend work list` column for column, plus the live STATE gauge the
/// CLI table cannot show: the point of reading this inside watch rather than
/// in a shell is seeing which Work is working right now.
pub(super) fn render_work_table(f: &mut Frame, area: Rect, app: &mut App, targets: &[TreeTarget]) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    if targets.is_empty() {
        render_empty_table(f, area, app, theme);
        return;
    }
    let columns = work_table_columns(area.width);
    let header =
        Row::new(columns.iter().map(|column| column.header())).style(theme.table_header_style());
    let spin = Spinner {
        frame: app.anim_frame,
        enabled: app.watch_cfg.spinner && icons_unicode(),
    };
    let workspace_names: HashMap<_, _> = app
        .topology
        .sessions
        .iter()
        .map(|session| (session.key.clone(), session.name.clone()))
        .collect();
    let rows: Vec<Row> = targets
        .iter()
        .filter_map(|target| {
            let node = app.topology.find(&target.key)?;
            let TopologyNodeRef::Window(window) = node else {
                return None;
            };
            let cells = columns
                .iter()
                .map(|column| match column {
                    WorkTableColumn::State => {
                        Cell::from(tree_state_cell(target, node, theme, spin))
                    }
                    WorkTableColumn::Work => Cell::from(window.name.clone()),
                    WorkTableColumn::Workspace => Cell::from(
                        workspace_names
                            .get(&window.key.session)
                            .cloned()
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    WorkTableColumn::Generation => Cell::from(
                        window
                            .pipeline_run
                            .as_ref()
                            .map_or_else(|| "-".to_string(), |run| run.generation.to_string()),
                    ),
                    WorkTableColumn::Aliases => Cell::from(work_alias_summary(window)),
                    WorkTableColumn::Done => Cell::from(work_done_cell(window)),
                    WorkTableColumn::Cwd => Cell::from(work_cwd_cell(window)),
                })
                .collect::<Vec<_>>();
            Some(Row::new(cells))
        })
        .collect();
    let table = Table::new(rows, columns.iter().map(|column| column.constraint()))
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style())
                .border_type(theme.border_type)
                .title(" Work "),
        )
        .row_highlight_style(theme.selected_style())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, area, &mut app.table_state);
}

pub(super) fn render_tree_table(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    tree_targets: &[TreeTarget],
) {
    render_topology_table(f, area, app, tree_targets, " Topology ");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TopologyTableColumn {
    State,
    Node,
    Age,
    Summary,
}

impl TopologyTableColumn {
    fn header(self) -> &'static str {
        match self {
            Self::State => "STATE",
            Self::Node => "SESSION / WINDOW / PANE",
            Self::Age => "AGE",
            Self::Summary => "SUMMARY",
        }
    }

    fn constraint(self) -> Constraint {
        match self {
            Self::State | Self::Age => Constraint::Length(7),
            Self::Node => Constraint::Min(24),
            Self::Summary => Constraint::Min(18),
        }
    }
}

pub(super) fn topology_table_columns(width: u16) -> Vec<TopologyTableColumn> {
    let mut columns = vec![TopologyTableColumn::State, TopologyTableColumn::Node];
    // SUMMARY carries the current work/subagent context, so it returns before
    // AGE as horizontal space grows. The final order remains the familiar
    // STATE / NODE / AGE / SUMMARY once every column fits.
    if width >= 70 {
        columns.push(TopologyTableColumn::Age);
    }
    if width >= 60 {
        columns.push(TopologyTableColumn::Summary);
    }
    columns
}

fn render_topology_table(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    targets: &[TreeTarget],
    title: &'static str,
) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    if targets.is_empty() {
        render_empty_table(f, area, app, theme);
        return;
    }
    let columns = topology_table_columns(area.width);
    let header =
        Row::new(columns.iter().map(|column| column.header())).style(theme.table_header_style());
    let now = OffsetDateTime::now_utc();
    let spin = Spinner {
        frame: app.anim_frame,
        enabled: app.watch_cfg.spinner && icons_unicode(),
    };
    let endpoint_count = app
        .topology
        .sessions
        .iter()
        .map(|session| &session.key.endpoint)
        .collect::<HashSet<_>>()
        .len();
    let rows: Vec<Row> = targets
        .iter()
        .filter_map(|target| {
            let node = app.topology.find(&target.key)?;
            let cells = columns
                .iter()
                .map(|column| match column {
                    TopologyTableColumn::State => {
                        Cell::from(tree_state_cell(target, node, theme, spin))
                    }
                    TopologyTableColumn::Node => {
                        Cell::from(tree_node_label(target, node, endpoint_count > 1, theme))
                    }
                    TopologyTableColumn::Age => Cell::from(tree_node_age(app, target, node, now)),
                    TopologyTableColumn::Summary => Cell::from(truncate_chars(
                        &tree_node_summary(target, node, app.watch_cfg.layout),
                        120,
                    )),
                })
                .collect::<Vec<_>>();
            Some(Row::new(cells))
        })
        .collect();
    let widths = columns.iter().map(|column| column.constraint());
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style())
                .border_type(theme.border_type)
                .title(title),
        )
        .row_highlight_style(theme.selected_style())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, area, &mut app.table_state);
}

/// Below this table width the SUMMARY column folds away entirely.
///
/// Squeezing every column keeps SUMMARY alive as a four-character scrap
/// while truncating the one column that identifies the row — at the 30/70
/// inspector split the session *names* were the casualty. Names are why
/// the table exists; a summary that narrow was already unreadable.
pub(super) const SUMMARY_MIN_TABLE_WIDTH: u16 = 60;
