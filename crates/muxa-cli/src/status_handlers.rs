use super::{
    all_panes, attend, theme, Agent, AgentKind, AgentState, Cell, CliTheme, Client,
    ColumnConstraint, Config, ContentArrangement, Context, IconSet, IsTerminal, OffsetDateTime,
    Result, Style, Table, TableTone, ThemeArg, UnicodeWidthChar, Width, Write,
    COMPACT_STATUS_TABLE_WIDTH, DEFAULT_TERMINAL_WIDTH, FULL_STATUS_TABLE_WIDTH,
    MAX_STATUS_PROMPT_WIDTH, MIN_STATUS_PROMPT_WIDTH, STATUS_LINE_IPC_TIMEOUT, UTF8_BORDERS_ONLY,
};

pub(super) async fn cmd_status(
    client: &Client,
    cfg: &Config,
    theme: Option<ThemeArg>,
    flat: bool,
    json: bool,
) -> Result<()> {
    let agents = client.snapshot().await?;
    if json {
        let inputs = muxa::active_backends()
            .into_iter()
            .map(|backend| muxa::TopologyInput::new(backend.kind(), backend.list_panes()))
            .collect();
        return print_status_json(&agents, inputs, OffsetDateTime::now_utc());
    }
    if agents.is_empty() {
        println!("no active agents");
        return Ok(());
    }
    // Resolve the full pane inventory once so `pane_display` does N lookups
    // against a slice instead of N backend calls. Every active host, not
    // just the one this shell runs in: from a plain terminal (or a tmux
    // shell) the agents live in rmux, and only the env-detected backend
    // would leave every row as a raw `rmux:%N`. One row per pane, so a
    // grouped session's `~view~` listing never names it.
    let panes = all_panes().await;
    let theme = theme::for_config(cfg, theme, use_colors());
    let now = OffsetDateTime::now_utc();
    if flat {
        print_table(&agents, &muxa::tmux::canonical_panes(&panes), now, theme);
        return Ok(());
    }
    // The same session / window / pane tree `status --json` and `watch`
    // show, built from every host's panes (grouped `~view~` sessions fold
    // onto their base). Row inputs are grouped by host like `--json`.
    let mut by_host: std::collections::BTreeMap<
        String,
        (muxa::HostKind, Vec<muxa::tmux::PaneInfo>),
    > = std::collections::BTreeMap::new();
    for pane in panes {
        let host = muxa::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(muxa::HostKind::Tmux);
        by_host
            .entry(host.to_string())
            .or_insert_with(|| (host, Vec::new()))
            .1
            .push(pane);
    }
    let inputs = by_host
        .into_values()
        .map(|(host, panes)| muxa::TopologyInput::new(host, panes))
        .collect();
    let topology = muxa::TopologySnapshot::build(now, inputs, agents);
    println!(
        "{}",
        render_status_tree(&topology, now, theme, terminal_width())
    );
    Ok(())
}

/// `muxa status` as a tree: each session, its windows, and the agents in
/// their panes, with the flat table's state / age / prompt columns.
/// Sessions and windows without an agent are left out; agents with no pane
/// close the list. Single-agent sessions and windows collapse to one row.
pub(super) fn render_status_tree(
    topology: &muxa::TopologySnapshot,
    now: OffsetDateTime,
    theme: CliTheme,
    terminal_width: usize,
) -> String {
    let (mut table, layout, name_width, prompt_width) = status_tree_table(theme, terminal_width);
    let rows = TreeRows {
        layout,
        name_width,
        prompt_width,
        theme,
        now,
    };
    for session in &topology.sessions {
        push_session_rows(&mut table, &rows, session);
    }
    if !topology.unassigned_agents.is_empty() {
        table.add_row(rows.group("(no pane)", String::new()));
        let count = topology.unassigned_agents.len();
        for (at, agent) in topology.unassigned_agents.iter().enumerate() {
            let name = if agent.kind == AgentKind::Task {
                agent.session_id.clone()
            } else {
                crate::watch::agent_kind_short(agent.kind).to_string()
            };
            table.add_row(rows.agent(format!("{}{name}", branch(at, count)), agent));
        }
    }
    format!("{table}")
}

/// Cell builders shared by every row of the status tree.
pub(super) struct TreeRows {
    pub(super) layout: StatusTableLayout,
    pub(super) name_width: usize,
    pub(super) prompt_width: usize,
    pub(super) theme: CliTheme,
    pub(super) now: OffsetDateTime,
}

impl TreeRows {
    pub(super) fn agent(&self, label: String, agent: &Agent) -> Vec<Cell> {
        let state = status_state_label(agent.state, self.layout).to_string();
        let headline = muxa::prompt_headline(agent.last_prompt.as_deref().unwrap_or("-"));
        let prompt = headline.lines().next().unwrap_or("");
        vec![
            Cell::new(truncate_cell(&label, self.name_width)),
            status_state_cell(&state, agent.state, self.theme),
            self.theme.right_cell(
                truncate_cell(&relative_time(self.now, agent.last_activity_at), 7),
                TableTone::Dim,
            ),
            Cell::new(truncate_cell(prompt, self.prompt_width)),
        ]
    }

    pub(super) fn group(&self, label: &str, summary: String) -> Vec<Cell> {
        vec![
            self.theme
                .cell(truncate_cell(label, self.name_width), TableTone::Header),
            Cell::new(""),
            Cell::new(""),
            self.theme
                .cell(truncate_cell(&summary, self.prompt_width), TableTone::Dim),
        ]
    }

    pub(super) fn label(&self, label: &str) -> Vec<Cell> {
        vec![
            Cell::new(truncate_cell(label, self.name_width)),
            Cell::new(""),
            Cell::new(""),
            Cell::new(""),
        ]
    }
}

pub(super) fn branch(at: usize, count: usize) -> &'static str {
    if at + 1 == count {
        "└─ "
    } else {
        "├─ "
    }
}

/// One session's rows. A session with one agent is a single row; with one
/// window its agents hang off the session; a window with one agent carries
/// it on the window's own row.
pub(super) fn push_session_rows(table: &mut Table, rows: &TreeRows, session: &muxa::SessionNode) {
    let kind = |agent: &Agent| crate::watch::agent_kind_short(agent.kind);
    let windows: Vec<_> = session
        .windows
        .iter()
        .filter_map(|window| {
            let agents: Vec<_> = window
                .panes
                .iter()
                .filter_map(|pane| pane.agent.as_ref().map(|agent| (pane, agent)))
                .collect();
            (!agents.is_empty()).then_some((window, agents))
        })
        .collect();
    match windows.as_slice() {
        [] => {}
        [(_, agents)] if agents.len() == 1 => {
            let agent = agents[0].1;
            table.add_row(rows.agent(format!("{} · {}", session.name, kind(agent)), agent));
        }
        [(_, agents)] => {
            table.add_row(rows.group(&session.name, state_summary(&session.states)));
            for (at, (pane, agent)) in agents.iter().enumerate() {
                let label = format!("{}{} {}", branch(at, agents.len()), pane.index, kind(agent));
                table.add_row(rows.agent(label, agent));
            }
        }
        _ => {
            table.add_row(rows.group(&session.name, state_summary(&session.states)));
            for (window_at, (window, agents)) in windows.iter().enumerate() {
                let window_branch = branch(window_at, windows.len());
                let window_label = format!("{window_branch}{}:{}", window.index, window.name);
                if let [(_, agent)] = agents.as_slice() {
                    table.add_row(rows.agent(format!("{window_label} · {}", kind(agent)), agent));
                    continue;
                }
                table.add_row(rows.label(&window_label));
                let indent = if window_at + 1 == windows.len() {
                    "   "
                } else {
                    "│  "
                };
                for (at, (pane, agent)) in agents.iter().enumerate() {
                    let label = format!(
                        "{indent}{}{} {}",
                        branch(at, agents.len()),
                        pane.index,
                        kind(agent)
                    );
                    table.add_row(rows.agent(label, agent));
                }
            }
        }
    }
}

/// The status table with NAME widened for tree prefixes: returns the table,
/// its layout, and the NAME and prompt widths its rows must truncate to.
pub(super) fn status_tree_table(
    theme: CliTheme,
    terminal_width: usize,
) -> (Table, StatusTableLayout, usize, usize) {
    let layout = status_table_layout(terminal_width);
    // Give the prefixes room from the prompt so window names stay readable.
    let widen = TREE_NAME_EXTRA.min(status_prompt_width(terminal_width, layout).saturating_sub(16));
    let prompt_width = status_prompt_width(terminal_width, layout) - widen;
    let name_width = match layout {
        StatusTableLayout::Full => 24,
        StatusTableLayout::Compact => 18,
        StatusTableLayout::Minimal => 14,
    } + widen;
    let mut constraints = status_table_constraints(layout, prompt_width);
    constraints[0] =
        ColumnConstraint::Absolute(Width::Fixed(u16::try_from(name_width).unwrap_or(u16::MAX)));
    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints(constraints)
        .set_header(status_table_header(layout, prompt_width, theme));
    (table, layout, name_width, prompt_width)
}

/// Extra NAME columns in tree mode, for the `│  ├─ ` prefixes.
pub(super) const TREE_NAME_EXTRA: usize = 8;

pub(super) fn state_summary(states: &muxa::StateDistribution) -> String {
    let waiting = states.waiting_input + states.waiting_choice;
    [
        (states.working, "working"),
        (waiting, "waiting"),
        (states.error, "error"),
        (states.idle, "idle"),
    ]
    .iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(" · ")
}

pub(super) fn status_json(
    agents: &[Agent],
    inputs: Vec<muxa::TopologyInput>,
    generated_at: OffsetDateTime,
) -> muxa::TopologySnapshot {
    muxa::TopologySnapshot::build(generated_at, inputs, agents.to_vec())
}

pub(super) fn print_status_json(
    agents: &[Agent],
    inputs: Vec<muxa::TopologyInput>,
    generated_at: OffsetDateTime,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, &status_json(agents, inputs, generated_at))
        .context("serializing status JSON")?;
    writeln!(out)?;
    Ok(())
}

/// Format the global attention segment for tmux `status-right`. Empty when
/// nothing is blocked so the segment vanishes when all-clear; otherwise
/// tmux-style color markup — never raw ANSI, since tmux renders `#[...]`
/// itself exactly like the per-pane path relies on.
pub(super) fn attention_segment(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        let verb = if count == 1 { "needs" } else { "need" };
        format!("#[fg=red]⚠ {count} {verb} you#[default]")
    }
}

pub(super) async fn cmd_status_line(
    client: &Client,
    pane: Option<String>,
    needs_attention: bool,
) -> Result<()> {
    // Global attention summary: a blocked agent in ANY pane surfaces
    // passively, even while you're focused on a different pane. Keep the
    // tight IPC deadline + empty-on-timeout contract of the status-line
    // path so a slow daemon can never stall the tmux status bar.
    if needs_attention {
        let agents = client
            .snapshot_with_timeout(STATUS_LINE_IPC_TIMEOUT)
            .await
            .unwrap_or_default();
        let count = agents
            .iter()
            .filter(|a| attend::needs_attention(a.state))
            .count();
        println!("{}", attention_segment(count));
        return Ok(());
    }

    let backend = muxa::default_backend();
    let pane = pane.or_else(|| backend.current_pane());
    let agents = match &pane {
        Some(p) => client
            .by_pane_with_timeout(p, STATUS_LINE_IPC_TIMEOUT)
            .await
            .unwrap_or_default(),
        None => client
            .snapshot_with_timeout(STATUS_LINE_IPC_TIMEOUT)
            .await
            .unwrap_or_default(),
    };
    if agents.is_empty() {
        println!();
        return Ok(());
    }

    let panes_snapshot = backend.list_panes();
    // tmux handles its own color markup, so we never emit ANSI here.
    let parts: Vec<String> = agents
        .iter()
        .map(|a| {
            let icon = state_icon(a.state);
            let kind = a.kind.to_string();
            // Prefer session:window when we can resolve it — makes the
            // status-line read "● main:2 claude_code" instead of a
            // context-free glyph.
            // Resolve against the snapshot fetched once above instead
            // of shelling out per agent.
            let loc = a.pane.as_deref().and_then(|raw| {
                panes_snapshot
                    .iter()
                    .find(|p| p.pane_id == raw)
                    .map(|p| format!(" {}:{}", p.session, p.window_index))
            });
            match loc {
                Some(l) => format!("{icon}{l} {kind}"),
                None => format!("{icon} {kind}"),
            }
        })
        .collect();
    println!("{}", parts.join(" | "));
    Ok(())
}

pub(super) async fn cmd_recap(
    client: &Client,
    pane: Option<String>,
    limit: usize,
    all: bool,
) -> Result<()> {
    let pane = pane
        .or_else(|| muxa::default_backend().current_pane())
        .context("no pane given and could not determine current pane (set $TMUX_PANE / $ZELLIJ_PANE_ID, or pass --pane)")?;

    let agents = client.by_pane(&pane).await?;
    let history_limit = if all { Some(0) } else { Some(limit) };
    let history = client
        .recent_prompts(Some(&pane), history_limit)
        .await
        .unwrap_or_default();

    if agents.is_empty() && history.is_empty() {
        println!("no agent or recorded prompts in pane {pane}");
        return Ok(());
    }

    for a in agents {
        let kind = a.kind.to_string();
        let state = a.state.to_string();
        let prompt = a.last_prompt.unwrap_or_else(|| "(none)".into());
        println!("── {kind}  [{state}] ────────────");
        println!("{prompt}");
        println!();
    }

    if !history.is_empty() {
        let count = history.len();
        let header = if all {
            format!("── recent prompts ({count} total) ────────────")
        } else {
            format!("── recent prompts (showing up to {limit}) ────────────")
        };
        println!("{header}");
        for entry in history {
            // Concise, scannable row: "<rfc3339-short>  <kind>  <prompt-first-line>"
            let stamp = entry
                .at
                .format(time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]"
                ))
                .unwrap_or_else(|_| entry.at.to_string());
            let first_line = entry.prompt.lines().next().unwrap_or("");
            println!("{stamp}  {}  {first_line}", entry.kind);
        }
    }
    Ok(())
}

pub(super) fn cmd_panes() -> Result<()> {
    // Aggregate across every active host — the cross-multiplexer pane
    // inventory. Rows carry their namespace in `pane_id`, so a concat keeps
    // tmux `%N` and herdr `herdr:…` rows distinct. Per-host hints still fire
    // for any host in the set that contributed zero panes, so a single-host
    // user sees the same diagnostic as before while a multi-host user learns
    // which side is empty.
    let backends = muxa::active_backends();
    let mut all: Vec<muxa::tmux::PaneInfo> = Vec::new();
    let mut empty_hosts: Vec<&muxa::SharedBackend> = Vec::new();
    for backend in &backends {
        let panes = backend.list_panes();
        if panes.is_empty() {
            empty_hosts.push(backend);
        }
        all.extend(panes);
    }

    let terminal_width = terminal_width();
    let mut out = std::io::stdout().lock();
    for line in pane_inventory_lines(&all) {
        if let Err(error) = writeln!(out, "{}", truncate_cell(&line, terminal_width)) {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("write pane inventory");
        }
    }

    // A host contributed zero: print its diagnostic. When the set is a
    // single host and it's empty this reproduces the pre-multi-host hint;
    // when other hosts have panes it tells the user which side came up dry.
    for backend in empty_hosts {
        let hint = empty_pane_hint(backend.as_ref());
        if let Err(error) = writeln!(out, "{hint}") {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("write pane inventory hint");
        }
    }
    Ok(())
}

/// `muxa panes` rows. A session group lists each pane once per `~view~`
/// member: show the pane once, under its base session, with how many views
/// share it.
pub(super) fn pane_inventory_lines(all: &[muxa::tmux::PaneInfo]) -> Vec<String> {
    let mut listings: std::collections::HashMap<(Option<String>, String), usize> =
        std::collections::HashMap::new();
    for pane in all {
        *listings
            .entry((pane.socket.clone(), pane.pane_id.clone()))
            .or_default() += 1;
    }
    muxa::tmux::canonical_panes(all)
        .iter()
        .map(|p| {
            let views = match listings
                .get(&(p.socket.clone(), p.pane_id.clone()))
                .map_or(0, |count| count - 1)
            {
                0 => String::new(),
                1 => "  +1 view".to_owned(),
                n => format!("  +{n} views"),
            };
            format!(
                "{:<8} {}:{}.{}{views}  tty={}  cmd={}  title={}",
                p.pane_id,
                p.session,
                p.window_index,
                p.pane_index,
                p.tty,
                p.current_command,
                p.title
            )
        })
        .collect()
}

/// The empty-state diagnostic for a host that reported no panes. Two ways
/// to get here: the host really has no panes, or the backend's `caps()`
/// says metadata is plugin-only and not pushed yet — the zellij branch
/// differentiates so a misconfigured plugin is diagnosable.
pub(super) fn empty_pane_hint(backend: &dyn muxa::PaneBackend) -> &'static str {
    match backend.kind() {
        muxa::HostKind::Tmux => "(no tmux panes — server may be down)",
        muxa::HostKind::Cmux => {
            "(cmux first slice: only the current env surface is visible; full inventory is pending)"
        }
        muxa::HostKind::Rmux => "(no rmux panes — server may be down or endpoint unreachable)",
        muxa::HostKind::Zellij if !backend.caps().current_command => {
            "(zellij CLI baseline: pane inventory is plugin-only — install the muxa zellij plugin to populate)"
        }
        muxa::HostKind::Zellij => "(no zellij panes)",
        muxa::HostKind::Herdr => {
            "(no herdr panes — server may be down or socket unreachable)"
        }
    }
}

/// Decide whether to emit ANSI color. We check `NO_COLOR` (per the de-facto
/// standard) and require stdout to be a TTY — piping `muxa status | grep`
/// should stay clean.
pub(crate) fn use_colors() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

pub(crate) fn terminal_width() -> usize {
    crossterm::terminal::size().map_or(DEFAULT_TERMINAL_WIDTH, |(width, _)| usize::from(width))
}

pub(crate) fn truncate_cell(value: &str, max_chars: usize) -> String {
    if value
        .chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum::<usize>()
        <= max_chars
    {
        return value.to_string();
    }

    if max_chars <= 3 {
        let mut used = 0;
        let mut out = String::new();
        for ch in value.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + width > max_chars {
                break;
            }
            used += width;
            out.push(ch);
        }
        return out;
    }

    let content_width = max_chars - 3;
    let mut used = 0;
    let mut out = String::new();
    for ch in value.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > content_width {
            break;
        }
        used += width;
        out.push(ch);
    }
    out.push_str("...");
    out
}

/// Process-wide glyph set, recorded once at startup from `[ui] icons`.
///
/// Mirrors `use_colors()` as a global display predicate so the pure
/// `state_icon` / `state_marker` helpers don't need a config parameter
/// threaded through every call site. Unset (e.g. in unit tests) defaults
/// to `Unicode`, preserving prior behavior.
pub(super) static ICON_SET: std::sync::OnceLock<IconSet> = std::sync::OnceLock::new();

pub(crate) fn set_icon_set(set: IconSet) {
    let _ = ICON_SET.set(set);
}

pub(crate) fn icon_set() -> IconSet {
    ICON_SET.get().copied().unwrap_or_default()
}

pub(crate) fn state_icon(state: AgentState) -> &'static str {
    match icon_set() {
        IconSet::Unicode => state_icon_unicode(state),
        IconSet::Ascii => state_icon_ascii(state),
    }
}

pub(super) fn state_icon_unicode(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "●",
        AgentState::Idle => "○",
        AgentState::WaitingInput => "▶",
        AgentState::WaitingChoice => "◆",
        AgentState::Error => "■",
        AgentState::Stopped => "×",
        AgentState::Starting => "◌",
    }
}

pub(super) fn state_icon_ascii(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "*",
        AgentState::Idle => "o",
        AgentState::WaitingInput => ">",
        AgentState::WaitingChoice => "?",
        AgentState::Error => "!",
        AgentState::Stopped => "x",
        AgentState::Starting => "~",
    }
}

pub(crate) fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Working => Style::new().green(),
        AgentState::WaitingInput => Style::new().yellow(),
        AgentState::WaitingChoice => Style::new().yellow().bold(),
        AgentState::Idle => Style::new().dimmed(),
        AgentState::Error => Style::new().red(),
        AgentState::Stopped => Style::new().dimmed().strikethrough(),
        AgentState::Starting => Style::new().cyan(),
    }
}

/// Pretty-print `Agent.pane` as `session:window.pane` by lookup against
/// a pre-fetched pane list, or fall back to the raw pane id (or `-` if
/// unknown).
///
/// **Why a slice and not a tmux shell-out**: this used to call
/// `tmux::resolve_pane` which itself shells out to `tmux list-panes
/// -a` per call. With 30+ agents that meant 30+ subprocess invocations
/// for one `muxa status` run. Callers now fetch the pane list once and
/// pass it down.
pub(crate) fn pane_display(a: &Agent, panes: &[muxa::tmux::PaneInfo]) -> String {
    let Some(raw) = a.pane.as_deref() else {
        // Paneless background tasks have no tmux location — show their
        // registered name instead of a context-free "-".
        if a.kind == AgentKind::Task {
            return a.session_id.clone();
        }
        return "-".to_string();
    };
    // Pane ids repeat across servers; when the row names its server, only
    // that server's pane is this one.
    let located = panes.iter().find(|p| {
        p.pane_id == raw
            && match (a.tmux_socket.as_deref(), p.socket.as_deref()) {
                (Some(agent), Some(pane)) => {
                    muxa::backend::pane_endpoints_match(Some(raw), agent, pane)
                }
                _ => true,
            }
    });
    match (located, a.tmux_session.as_deref()) {
        (Some(p), _) => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
        // No live listing (host unreachable from this shell): the session
        // the daemon recorded still says where the agent is.
        (None, Some(session)) if !session.is_empty() => format!(
            "{}:{}",
            muxa::tmux::base_session_name(session),
            raw.rsplit(':').next().unwrap_or(raw)
        ),
        (None, _) => raw.to_string(),
    }
}

/// Human-friendly delta between `then` and `now`, rounded to the largest
/// unit. Past timestamps only — future clocks collapse to "0s ago".
pub(crate) fn relative_time(now: OffsetDateTime, then: OffsetDateTime) -> String {
    let delta = now - then;
    let secs = delta.whole_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

pub(super) fn print_table(
    agents: &[Agent],
    panes: &[muxa::tmux::PaneInfo],
    now: OffsetDateTime,
    theme: CliTheme,
) {
    println!(
        "{}",
        render_status_table(agents, panes, now, theme, terminal_width())
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusTableLayout {
    Full,
    Compact,
    Minimal,
}

pub(super) fn render_status_table(
    agents: &[Agent],
    panes: &[muxa::tmux::PaneInfo],
    now: OffsetDateTime,
    theme: CliTheme,
    terminal_width: usize,
) -> String {
    let layout = status_table_layout(terminal_width);
    let prompt_width = status_prompt_width(terminal_width, layout);
    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints(status_table_constraints(layout, prompt_width))
        .set_header(status_table_header(layout, prompt_width, theme));

    for a in agents {
        let pane = pane_display(a, panes);
        let state_txt = status_state_label(a.state, layout).to_string();
        let last_activity = relative_time(now, a.last_activity_at);
        let prompt_raw = muxa::prompt_headline(a.last_prompt.as_deref().unwrap_or("-"));
        let prompt = prompt_raw.lines().next().unwrap_or("");

        let state_cell = status_state_cell(&state_txt, a.state, theme);
        let row = match layout {
            StatusTableLayout::Full => vec![
                Cell::new(truncate_cell(&pane, 24)),
                state_cell,
                theme.right_cell(truncate_cell(&last_activity, 7), TableTone::Dim),
                Cell::new(truncate_cell(prompt, prompt_width)),
            ],
            StatusTableLayout::Compact => vec![
                Cell::new(truncate_cell(&pane, 18)),
                state_cell,
                theme.right_cell(truncate_cell(&last_activity, 7), TableTone::Dim),
                Cell::new(truncate_cell(prompt, prompt_width)),
            ],
            StatusTableLayout::Minimal => vec![
                Cell::new(truncate_cell(&pane, 14)),
                state_cell,
                theme.right_cell(truncate_cell(&last_activity, 7), TableTone::Dim),
                Cell::new(truncate_cell(prompt, prompt_width)),
            ],
        };
        table.add_row(row);
    }

    format!("{table}")
}

pub(super) fn status_table_layout(terminal_width: usize) -> StatusTableLayout {
    if terminal_width >= FULL_STATUS_TABLE_WIDTH {
        StatusTableLayout::Full
    } else if terminal_width >= COMPACT_STATUS_TABLE_WIDTH {
        StatusTableLayout::Compact
    } else {
        StatusTableLayout::Minimal
    }
}

pub(super) fn status_table_constraints(
    layout: StatusTableLayout,
    prompt_width: usize,
) -> Vec<ColumnConstraint> {
    // Column order matches `status_table_header`: NAME, ST, ACT, LAST PROMPT.
    // Per-layout widths are the NAME / ST / ACT cells; the prompt cell
    // soaks up whatever's left and is provided by the caller.
    let widths: &[usize] = match layout {
        StatusTableLayout::Full => &[24, 14, 7],
        StatusTableLayout::Compact => &[18, 7, 7],
        StatusTableLayout::Minimal => &[14, 6, 7],
    };
    widths
        .iter()
        .copied()
        .chain(std::iter::once(prompt_width))
        .map(|width| {
            ColumnConstraint::Absolute(Width::Fixed(u16::try_from(width).unwrap_or(u16::MAX)))
        })
        .collect()
}

pub(super) fn status_table_header(
    layout: StatusTableLayout,
    prompt_width: usize,
    theme: CliTheme,
) -> Vec<Cell> {
    // Default columns: NAME / ST / ACT / LAST PROMPT across every layout.
    // KIND and MODEL used to share the row but were demoted to opt-in
    // because the prompt is the highest-value column on every screen
    // size — leading with identity + state + age + content keeps the
    // narrow-terminal path readable without losing parity with wide
    // terminals.
    let prompt_label = if matches!(layout, StatusTableLayout::Full) {
        "LAST PROMPT"
    } else {
        "PROMPT"
    };
    vec![
        theme.cell("NAME", TableTone::Header),
        theme.cell("ST", TableTone::Header),
        theme.right_cell("ACT", TableTone::Header),
        theme.cell(truncate_cell(prompt_label, prompt_width), TableTone::Header),
    ]
}

pub(super) fn status_prompt_width(terminal_width: usize, layout: StatusTableLayout) -> usize {
    // Sum of the non-prompt column widths declared in
    // `status_table_constraints` — keep these in sync.
    let fixed_width: usize = match layout {
        StatusTableLayout::Full => 24 + 14 + 7,
        StatusTableLayout::Compact => 18 + 7 + 7,
        StatusTableLayout::Minimal => 14 + 6 + 7,
    };
    // Every layout is now 4 columns (NAME / ST / ACT / LAST PROMPT).
    let column_count: usize = 4;
    let border_and_padding_width = column_count + 1 + column_count * 2;
    terminal_width
        .saturating_sub(fixed_width + border_and_padding_width)
        .clamp(MIN_STATUS_PROMPT_WIDTH, MAX_STATUS_PROMPT_WIDTH)
}

pub(super) fn status_state_label(state: AgentState, layout: StatusTableLayout) -> &'static str {
    if layout == StatusTableLayout::Full {
        return match state {
            AgentState::Working => "working",
            AgentState::Idle => "idle",
            AgentState::WaitingInput => "waiting_input",
            AgentState::WaitingChoice => "waiting_choice",
            AgentState::Error => "error",
            AgentState::Stopped => "stopped",
            AgentState::Starting => "starting",
        };
    }
    match state {
        AgentState::Working => "work",
        AgentState::Idle => "idle",
        AgentState::WaitingInput => "input",
        AgentState::WaitingChoice => "choice",
        AgentState::Error => "error",
        AgentState::Stopped => "stop",
        AgentState::Starting => "start",
    }
}

pub(super) fn status_state_cell(label: &str, state: AgentState, theme: CliTheme) -> Cell {
    theme.state_cell(label, state)
}
