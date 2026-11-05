use super::{
    icons_unicode, pipeline_status_label, relative_time, row_host, state_sort_rank, Agent,
    AgentState, App, Color, Duration, Line, Modifier, OffsetDateTime, PaneInfo, PaneKey,
    SortContext, Span, Style, Text, WatchRow, WatchSortKey, WatchThemeSpec, WorkRow, SWARM_DOTS,
    SWARM_START,
};

/// Whether the visible row set spans more than one host — the trigger for
/// showing per-row host badges. Single-host users (the common case) see no
/// badge, so nothing changes for them. Rows with no classifiable host don't
/// count toward the distinct-host tally.
pub(super) fn rows_multi_host(rows: &[WatchRow]) -> bool {
    let mut seen: Option<muxa::HostKind> = None;
    for row in rows {
        if let Some(host) = row_host(row) {
            match seen {
                Some(prev) if prev != host => return true,
                None => seen = Some(host),
                Some(_) => {}
            }
        }
    }
    false
}

/// The subtle dim host tag shown before a multi-host row's SESSION/PANE
/// cell. Mirrors the dashboard TUI's `CardHost` naming.
fn host_badge_label(host: muxa::HostKind) -> &'static str {
    match host {
        muxa::HostKind::Tmux => "tmux",
        muxa::HostKind::Cmux => "cmux",
        muxa::HostKind::Rmux => "rmux",
        muxa::HostKind::Zellij => "zellij",
        muxa::HostKind::Herdr => "herdr",
    }
}

/// Prepend the dim host tag to a cell's first line. Only called when the
/// row set spans multiple hosts, so the badge disambiguates rather than
/// adding noise.
pub(super) fn prepend_host_badge(text: &mut Text<'_>, host: muxa::HostKind) {
    let style = Style::default().add_modifier(Modifier::DIM);
    let badge = Span::styled(format!("{} ", host_badge_label(host)), style);
    match text.lines.first_mut() {
        Some(line) => line.spans.insert(0, badge),
        None => text.lines.push(Line::from(badge)),
    }
}

pub(super) fn prepend_tree_prefix(text: &mut Text<'_>, prefix: &str, style: Style) {
    let prefix = Span::styled(prefix.to_string(), style);
    match text.lines.first_mut() {
        Some(line) => line.spans.insert(0, prefix),
        None => text.lines.push(Line::from(prefix)),
    }
}

pub(super) fn sort_panes(a: &PaneInfo, b: &PaneInfo) -> std::cmp::Ordering {
    a.session
        .cmp(&b.session)
        .then_with(|| {
            a.window_index
                .parse::<u32>()
                .unwrap_or(u32::MAX)
                .cmp(&b.window_index.parse::<u32>().unwrap_or(u32::MAX))
        })
        .then_with(|| {
            a.pane_index
                .parse::<u32>()
                .unwrap_or(u32::MAX)
                .cmp(&b.pane_index.parse::<u32>().unwrap_or(u32::MAX))
        })
        .then_with(|| {
            let host_a =
                muxa::backend::pane_id_host_kind(&a.pane_id).unwrap_or(muxa::HostKind::Tmux);
            let host_b =
                muxa::backend::pane_id_host_kind(&b.pane_id).unwrap_or(muxa::HostKind::Tmux);
            PaneKey::from_pane(host_a, a).cmp(&PaneKey::from_pane(host_b, b))
        })
}

pub(super) fn sort_works(
    a: &WorkRow,
    b: &WorkRow,
    keys: &[WatchSortKey],
    sort_context: &SortContext<'_>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let pane_info = |row: &WorkRow| {
        row.window_key
            .as_ref()
            .and_then(|key| sort_context.pane_for_window(key))
            .or_else(|| {
                row.representative_pane
                    .as_deref()
                    .and_then(|id| sort_context.pane(id))
            })
    };
    for key in keys {
        let cmp = match key {
            WatchSortKey::Name => a.session.cmp(&b.session),
            WatchSortKey::Activity => b
                .latest_agent
                .as_ref()
                .map(|agent| agent.last_activity_at)
                .cmp(&a.latest_agent.as_ref().map(|agent| agent.last_activity_at)),
            WatchSortKey::State => {
                let rank = |row: &WorkRow| {
                    row.latest_agent
                        .as_ref()
                        .map_or(u8::MAX, |agent| state_sort_rank(agent.state))
                };
                rank(a).cmp(&rank(b))
            }
            WatchSortKey::Duration => sort_context
                .session_duration_secs(&b.session)
                .cmp(&sort_context.session_duration_secs(&a.session)),
            WatchSortKey::Pane => {
                let key_for = |info: Option<&PaneInfo>| {
                    info.map(|p| {
                        (
                            p.window_index.parse::<u32>().unwrap_or(u32::MAX),
                            p.pane_index.parse::<u32>().unwrap_or(u32::MAX),
                        )
                    })
                };
                key_for(pane_info(a)).cmp(&key_for(pane_info(b)))
            }
            WatchSortKey::PaneId => a.representative_pane.cmp(&b.representative_pane),
        };
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
    // Final tiebreak on the host-namespaced key so two rows sharing a raw
    // session id (tmux "w1" + herdr "w1") get a stable, deterministic order.
    a.group_key.cmp(&b.group_key)
}

/// Render a `pane_id` as `session:window.pane` when we can resolve it
/// against the cached pane list, falling back to the raw id (e.g.
/// `%1618`) when the agent's pane no longer exists.
///
/// **Why a slice and not a tmux shell-out**: this function runs once
/// per agent row per render frame. Shelling out to `tmux list-panes`
/// per call cost ~5 ms each, so a 35-agent table at 60 Hz target
/// blocked the input loop for ~175 ms per frame. The refresh task
/// already caches the full pane inventory in `App::panes`; we read
/// from there instead.
pub(super) fn pane_display(pane_id: Option<&str>, panes: &[PaneInfo]) -> String {
    pane_display_on(pane_id, None, panes)
}

pub(super) fn agent_pane_display(agent: &Agent, panes: &[PaneInfo]) -> String {
    pane_display_on(agent.pane.as_deref(), agent.tmux_socket.as_deref(), panes)
}

fn pane_display_on(pane_id: Option<&str>, socket: Option<&str>, panes: &[PaneInfo]) -> String {
    let Some(id) = pane_id else {
        // Distinguished from "—" / dash placeholders elsewhere so users
        // can see at a glance which agents have no tmux attachment to
        // jump into. Common case: Claude SDK sub-process whose env
        // didn't carry TMUX_PANE and whose process ancestry walk also
        // failed to find a pane.
        return "(no pane)".into();
    };
    let candidates = muxa::tmux::canonical_panes(
        &panes
            .iter()
            .filter(|pane| pane.pane_id == id)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let candidates: Vec<&PaneInfo> = candidates.iter().collect();
    let matched = match socket {
        Some(endpoint) => candidates.iter().copied().find(|pane| {
            let host =
                muxa::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(muxa::HostKind::Tmux);
            let pane_endpoint = PaneKey::from_pane(host, pane)
                .window
                .session
                .endpoint
                .socket;
            muxa::backend::pane_endpoint_identity(Some(id), endpoint) == pane_endpoint
        }),
        None if candidates.len() == 1 => Some(candidates[0]),
        None => None,
    };
    match matched {
        Some(p) => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
        None => id.to_string(),
    }
}

const STATE_SUMMARY_ORDER: [AgentState; 7] = [
    AgentState::Error,
    AgentState::WaitingInput,
    AgentState::WaitingChoice,
    AgentState::Working,
    AgentState::Starting,
    AgentState::Idle,
    AgentState::Stopped,
];

pub(super) const WORK_STATE_GUTTER_WIDTH: usize = 6;
const WORK_STATE_GUTTER_CONTENT_WIDTH: usize = WORK_STATE_GUTTER_WIDTH - 1;

#[derive(Clone)]
struct StateSummaryPart {
    label: String,
    style: Style,
    count: usize,
}

pub(super) fn state_summary_spans(
    states: impl IntoIterator<Item = AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<Span<'static>> {
    state_summary_parts(states, theme, spin)
        .into_iter()
        .enumerate()
        .flat_map(|(i, part)| {
            let mut spans = Vec::with_capacity(2);
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(part.label, part.style));
            spans
        })
        .collect()
}

fn state_summary_parts(
    states: impl IntoIterator<Item = AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<StateSummaryPart> {
    let states: Vec<AgentState> = states.into_iter().collect();
    let mut parts = Vec::new();

    for state in STATE_SUMMARY_ORDER {
        let count = states.iter().filter(|&&seen| seen == state).count();
        if count == 0 {
            continue;
        }
        let (symbol, style) = state_marker(state, theme, spin);
        let label = if count == 1 {
            symbol.to_string()
        } else {
            format!("{symbol}{count}")
        };
        parts.push(StateSummaryPart {
            label,
            style,
            count,
        });
    }

    parts
}

fn state_summary_parts_width(parts: &[StateSummaryPart]) -> usize {
    let labels = parts
        .iter()
        .map(|part| unicode_width::UnicodeWidthStr::width(part.label.as_str()))
        .sum::<usize>();
    let spaces = parts.len().saturating_sub(1);
    labels + spaces
}

fn overflow_label(count: usize, max_width: usize) -> String {
    let label = format!("+{count}");
    if unicode_width::UnicodeWidthStr::width(label.as_str()) <= max_width {
        label
    } else {
        "+".to_string()
    }
}

pub(super) fn state_summary_gutter_spans(
    states: impl IntoIterator<Item = AgentState>,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Vec<Span<'static>> {
    let parts = state_summary_parts(states, theme, spin);
    let fitted = if state_summary_parts_width(&parts) <= WORK_STATE_GUTTER_CONTENT_WIDTH {
        parts
    } else {
        let total_count = parts.iter().map(|part| part.count).sum::<usize>();
        let mut kept = Vec::new();
        let mut omitted_count = total_count;

        for (i, part) in parts.iter().enumerate() {
            let remaining_count = parts[i + 1..].iter().map(|part| part.count).sum::<usize>();
            let mut candidate = kept.clone();
            candidate.push(part.clone());
            let overflow_width = if remaining_count == 0 {
                0
            } else {
                let separator = usize::from(!candidate.is_empty());
                let label = overflow_label(remaining_count, WORK_STATE_GUTTER_CONTENT_WIDTH);
                separator + unicode_width::UnicodeWidthStr::width(label.as_str())
            };
            if state_summary_parts_width(&candidate) + overflow_width
                <= WORK_STATE_GUTTER_CONTENT_WIDTH
            {
                kept = candidate;
                omitted_count = remaining_count;
            } else {
                break;
            }
        }

        if omitted_count > 0 {
            kept.push(StateSummaryPart {
                label: overflow_label(omitted_count, WORK_STATE_GUTTER_CONTENT_WIDTH),
                style: theme.dim_style(),
                count: omitted_count,
            });
        }
        kept
    };

    let mut spans = state_summary_spans_from_parts(&fitted);
    let width = spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    if width < WORK_STATE_GUTTER_WIDTH {
        spans.push(Span::raw(" ".repeat(WORK_STATE_GUTTER_WIDTH - width)));
    }
    spans
}

#[cfg(test)]
mod pane_label_tests {
    use super::super::render_tree::pane_label_owner;
    use super::*;
    use muxa::topology::{BackendEndpoint, PaneKey, PaneNode, SessionKey, WindowKey};

    fn pane(alias: Option<&str>, command: &str, agent: Option<Agent>) -> PaneNode {
        PaneNode {
            key: PaneKey {
                window: WindowKey {
                    session: SessionKey {
                        endpoint: BackendEndpoint {
                            host: muxa::HostKind::Tmux,
                            socket: "default".into(),
                        },
                        session_id: "$1".into(),
                    },
                    window_id: "@1".into(),
                },
                pane_id: "%1".into(),
            },
            index: "0".into(),
            tty: String::new(),
            current_command: command.into(),
            title: String::new(),
            cwd: "/repo".into(),
            pane_pid: 0,
            agent,
            agent_alias: alias.map(Into::into),
        }
    }

    /// The case the program name cannot express: one work, two claudes. Both
    /// rows read `claude` and the tree stops telling them apart.
    #[test]
    fn the_pipeline_alias_names_the_pane() {
        assert_eq!(
            pane_label_owner(&pane(Some("impl"), "claude", None)),
            "impl"
        );
        assert_eq!(
            pane_label_owner(&pane(Some("review"), "claude", None)),
            "review",
        );
    }

    /// A pane nobody aliased still says what is running in it.
    #[test]
    fn an_unaliased_pane_falls_back_to_its_command() {
        assert_eq!(pane_label_owner(&pane(None, "vim", None)), "vim");
    }
}

#[cfg(test)]
mod work_up_key_tests {
    use super::super::input::{open_work_up_window, shell_quote, work_up_window_args};
    use super::super::{handle_work_composer_event, Action, KeyCode, KeyModifiers, WorkComposer};
    use super::*;

    /// `normalize_work_id` only rejects tabs, newlines and over-long input, so
    /// anything else a work id carries reaches a shell. It must arrive as one
    /// argument, not as a second command.
    #[test]
    fn a_work_id_cannot_break_out_of_the_shell_command() {
        let nasty = "cal-1'; rm -rf ~; echo '";
        let args = work_up_window_args("/usr/bin/muxa", nasty);
        let command = args.last().expect("command").clone();
        // Every embedded quote is closed, escaped and reopened, so the whole
        // id stays one word.
        assert!(command.contains(&shell_quote(nasty)), "{command}");
        // The only `; printf` is the one this function wrote.
        assert_eq!(command.matches("; printf").count(), 1, "{command}");
        assert!(!command.contains("; rm -rf ~; echo ';"), "{command}");
    }

    /// An install path with a space is ordinary on macOS.
    #[test]
    fn the_binary_path_is_quoted_too() {
        let command = work_up_window_args("/Applications/My Tools/muxa", "CAL-1")
            .last()
            .expect("command")
            .clone();
        assert!(
            command.starts_with("'/Applications/My Tools/muxa' work up 'CAL-1'"),
            "{command}",
        );
    }

    /// Outside tmux the window would be created somewhere the operator is not
    /// looking, and the run would sit on a confirmation prompt nobody sees.
    #[test]
    fn outside_tmux_it_refuses_and_names_the_command() {
        let error = open_work_up_window("cal-7229", false).expect_err("must refuse");
        assert!(error.contains("muxa work up CAL-7229"), "{error}");
    }

    /// The window has to outlive the command, or the plan `work up` prints
    /// scrolls past before it can be read.
    #[test]
    fn the_window_waits_before_closing() {
        let args = work_up_window_args("/usr/bin/muxa", "CAL-1");
        assert_eq!(args[0], "new-window");
        assert!(args.last().expect("command").ends_with("read _"));
    }

    /// Enter on an empty field must not spawn anything.
    #[test]
    fn an_empty_work_id_is_refused_before_it_spends_anything() {
        let mut app = App::new();
        app.work_composer = Some(WorkComposer::default());
        assert!(matches!(
            handle_work_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
            Action::None
        ));
        assert!(app.work_composer.is_some(), "the composer stays open");

        app.work_composer = Some(WorkComposer {
            input: "cal-7229".into(),
            cursor: 8,
        });
        assert!(matches!(
            handle_work_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
            Action::SubmitWorkUp
        ));
    }

    /// Backspace on an empty field closes the composer, matching `a`.
    #[test]
    fn backspace_on_an_empty_field_closes_the_composer() {
        let mut app = App::new();
        app.work_composer = Some(WorkComposer::default());
        assert!(matches!(
            handle_work_composer_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app),
            Action::None
        ));
        assert!(app.work_composer.is_none());
    }
}

fn state_summary_spans_from_parts(parts: &[StateSummaryPart]) -> Vec<Span<'static>> {
    parts
        .iter()
        .enumerate()
        .flat_map(|(i, part)| {
            let mut spans = Vec::with_capacity(2);
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(part.label.clone(), part.style));
            spans
        })
        .collect()
}

pub(super) fn work_label(s: &WorkRow, theme: WatchThemeSpec, spin: Spinner) -> Text<'static> {
    let mut spans = state_summary_gutter_spans(s.agent_states.values().copied(), theme, spin);
    spans.push(Span::raw(s.display_name.clone()));
    if let Some((done, total)) = s.completion {
        // Complete reads as a finished thing, not as another dim detail: this
        // badge is the only place the TUI says a pipeline converged.
        let style = if done == total {
            Style::default().fg(theme.state_idle)
        } else {
            Style::default().fg(theme.dim)
        };
        spans.push(Span::styled(format!("  {done}/{total}"), style));
    }
    if let Some(run) = s.pipeline_run.as_ref() {
        spans.push(Span::styled(
            format!("  {}", pipeline_status_label(run)),
            theme.dim_style(),
        ));
    }

    Text::from(Line::from(spans))
}

/// Per-frame animated state glyph for the `muxa watch` TUI. `enabled` mirrors
/// the `[watch] spinner` toggle; when off (and on every non-watch surface) the
/// static `state_icon` is used. Only `Working`/`Starting` animate.
#[derive(Clone, Copy)]
pub(super) struct Spinner {
    pub(super) frame: usize,
    pub(super) enabled: bool,
}

impl Spinner {
    #[cfg(test)]
    pub(super) const OFF: Spinner = Spinner {
        frame: 0,
        enabled: false,
    };

    fn glyph(self, state: AgentState) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        match state {
            AgentState::Working => Some(SWARM_DOTS[self.frame % SWARM_DOTS.len()]),
            AgentState::Starting => Some(SWARM_START[self.frame % SWARM_START.len()]),
            _ => None,
        }
    }
}

pub(super) fn state_marker(
    state: AgentState,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> (&'static str, Style) {
    // The static glyph is the shared source of truth with `muxa
    // status`/`status-line` (honoring the `[ui] icons` toggle); the spinner
    // only overrides `working`/`starting` inside the watch TUI.
    let icon = spin
        .glyph(state)
        .unwrap_or_else(|| crate::state_icon(state));
    (icon, theme.state_style(state))
}

pub(super) fn state_age_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Error => "ERR",
        AgentState::WaitingChoice => "CHOICE",
        AgentState::WaitingInput => "WAIT",
        AgentState::Working => "WORK",
        AgentState::Starting => "START",
        AgentState::Idle => "IDLE",
        AgentState::Stopped => "STOP",
    }
}

pub(super) fn state_age_text(
    agent: &Agent,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Text<'static> {
    let (symbol, style) = state_marker(agent.state, theme, spin);
    Text::from(Line::from(vec![
        Span::styled(format!("{symbol} "), style),
        Span::styled(
            format!(
                "{} {}",
                state_age_label(agent.state),
                relative_time(agent.state_entered_at, now)
            ),
            style,
        ),
    ]))
}

pub(super) fn session_state_age_text(
    session: &WorkRow,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    spin: Spinner,
) -> Text<'static> {
    let agent = session.agents.iter().min_by(|a, b| {
        state_sort_rank(a.state)
            .cmp(&state_sort_rank(b.state))
            .then_with(|| b.last_activity_at.cmp(&a.last_activity_at))
    });
    agent.map_or_else(
        || Text::from(Span::styled("—", theme.dim_style())),
        |agent| state_age_text(agent, now, theme, spin),
    )
}

/// A one-shot state-transition flash on an agent's State cell — a green `✓`
/// when a turn finishes (`working → idle`), a red flash when it errors. Unlike
/// the spinner (continuous, active states), a pulse marks a discrete *event*
/// and then decays to the static glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PulseKind {
    Done,
    Error,
}

#[derive(Clone, Copy)]
pub(super) struct Pulse {
    pub(super) kind: PulseKind,
    pub(super) started: std::time::Instant,
}

/// How long a transition pulse stays lit before decaying to the static glyph.
pub(super) const PULSE_WINDOW: Duration = Duration::from_millis(1300);

/// Classify a state transition into a pulse, if any. `prev == None` (first
/// sight, e.g. initial load) never pulses so the fleet doesn't flash on open.
pub(super) fn pulse_kind(prev: Option<AgentState>, cur: AgentState) -> Option<PulseKind> {
    let prev = prev?;
    match cur {
        AgentState::Error if prev != AgentState::Error => Some(PulseKind::Error),
        AgentState::Idle if prev == AgentState::Working => Some(PulseKind::Done),
        _ => None,
    }
}

/// Glyph + style for an active pulse. Flashes via reverse-video on a ~2-frame
/// cadence, then the pulse expires and the cell reverts to its static glyph.
/// Honors `[ui] icons = ascii` for the `✓`.
pub(super) fn pulse_glyph_style(
    kind: PulseKind,
    theme: WatchThemeSpec,
    frame: usize,
) -> (String, Style) {
    let (glyph, color) = match kind {
        PulseKind::Done => {
            let g = if icons_unicode() { "✓" } else { "+" };
            (g.to_string(), theme.state_working)
        }
        PulseKind::Error => (
            crate::state_icon(AgentState::Error).to_string(),
            theme.state_error,
        ),
    };
    let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    if (frame / 2).is_multiple_of(2) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    (glyph, style)
}

pub(super) fn pulse_cell(kind: PulseKind, theme: WatchThemeSpec, frame: usize) -> Text<'static> {
    let (glyph, style) = pulse_glyph_style(kind, theme, frame);
    Text::from(Span::styled(glyph, style))
}

/// Active pulse per row (by index), resolved outside the render closure so it
/// borrows `app` immutably before the stateful table render takes
/// `&mut app.table_state`.
pub(super) fn resolve_row_pulses(app: &App) -> Vec<Option<PulseKind>> {
    let now = std::time::Instant::now();
    app.rows
        .iter()
        .map(|r| {
            let who = match r {
                WatchRow::Agent(a) => Some((a.kind, a.session_id.as_str())),
                WatchRow::Work(s) => s
                    .latest_agent
                    .as_ref()
                    .map(|a| (a.kind, a.session_id.as_str())),
                WatchRow::BarePane(_) => None,
            };
            who.and_then(|(kind, sid)| app.active_pulse(kind, sid, now))
        })
        .collect()
}

/// True when any row holds a `working`/`starting` agent — i.e. something the
/// spinner would animate. Gates the fast repaint cadence so an all-idle fleet
/// keeps the calm 1 s idle repaint.
pub(super) fn rows_have_active_spinner(rows: &[WatchRow]) -> bool {
    fn is_spinner_state(state: AgentState) -> bool {
        matches!(state, AgentState::Working | AgentState::Starting)
    }
    rows.iter().any(|r| match r {
        WatchRow::Agent(a) => is_spinner_state(a.state),
        WatchRow::Work(s) => s.agents.iter().any(|a| is_spinner_state(a.state)),
        WatchRow::BarePane(_) => false,
    })
}

pub(super) fn duration_text(s: &WorkRow, now: OffsetDateTime) -> Text<'static> {
    let Some(activity) = s.activity.as_ref() else {
        return Text::from(Span::styled(
            "-",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    };
    let text = format_duration(activity.effective_total_secs(now));
    let style = if activity.is_attached() {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Text::from(Span::styled(text, style))
}

pub(super) fn format_duration(total_secs: u64) -> String {
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let minutes = total_secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let mins = minutes % 60;
    if hours < 24 {
        return format!("{hours}h{mins:02}m");
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    format!("{days}d{rem_hours:02}h")
}

pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
