use super::{
    agent_pane_display, duration_text, limits_text, plural, relative_time, session_state_age_text,
    state_age_text, state_marker, summary_line, work_label, work_workload_text, workload_text,
    Agent, AgentState, BorderType, Color, Constraint, Modifier, OffsetDateTime, PaneInfo, Span,
    Spinner, Style, Text, WatchConfig, WatchSummary, WatchTheme, WatchView, WidthSpec, WorkRow,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchThemeSpec {
    pub(super) title: &'static str,
    pub(super) accent: Color,
    accent_fg: Color,
    pub(super) action: Color,
    action_fg: Color,
    key_bg: Color,
    key_fg: Color,
    border: Color,
    pub(super) dim: Color,
    table_header: Color,
    selected_bg: Color,
    selected_fg: Option<Color>,
    pub(super) state_idle: Color,
    pub(super) state_working: Color,
    state_waiting: Color,
    state_choice: Color,
    pub(super) state_error: Color,
    state_starting: Color,
    pub(crate) border_type: BorderType,
}

impl WatchThemeSpec {
    pub(crate) fn accent_badge(self) -> Style {
        Style::default()
            .fg(self.accent_fg)
            .bg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub(crate) fn action_badge(self) -> Style {
        Style::default().fg(self.action_fg).bg(self.action)
    }

    pub(crate) fn key_badge(self) -> Style {
        Style::default().fg(self.key_fg).bg(self.key_bg)
    }

    pub(crate) fn border_style(self) -> Style {
        Style::default().fg(self.border)
    }

    pub(crate) fn dim_style(self) -> Style {
        Style::default().fg(self.dim)
    }

    pub(crate) fn table_header_style(self) -> Style {
        Style::default()
            .fg(self.table_header)
            .add_modifier(Modifier::BOLD)
    }

    pub(crate) fn selected_style(self) -> Style {
        let style = Style::default()
            .bg(self.selected_bg)
            .add_modifier(Modifier::BOLD);
        if let Some(fg) = self.selected_fg {
            style.fg(fg)
        } else {
            style
        }
    }

    pub(crate) fn state_style(self, state: AgentState) -> Style {
        match state {
            AgentState::Idle => Style::default()
                .fg(self.state_idle)
                .add_modifier(Modifier::BOLD),
            AgentState::Working => Style::default()
                .fg(self.state_working)
                .add_modifier(Modifier::BOLD),
            AgentState::WaitingInput => Style::default()
                .fg(self.state_waiting)
                .add_modifier(Modifier::BOLD),
            AgentState::WaitingChoice => Style::default()
                .fg(self.state_choice)
                .add_modifier(Modifier::BOLD),
            AgentState::Error => Style::default()
                .fg(self.state_error)
                .add_modifier(Modifier::BOLD),
            AgentState::Starting => Style::default().fg(self.state_starting),
            AgentState::Stopped => Style::default().fg(self.dim).add_modifier(Modifier::DIM),
        }
    }
}

pub(crate) fn watch_theme(theme: WatchTheme) -> WatchThemeSpec {
    match theme {
        WatchTheme::Classic => classic_watch_theme(),
        WatchTheme::OhMyTend => oh_my_tend_watch_theme(),
        WatchTheme::Focus => focus_watch_theme(),
        WatchTheme::Ops => ops_watch_theme(),
        WatchTheme::Mono => mono_watch_theme(),
        WatchTheme::HighContrast => high_contrast_watch_theme(),
        WatchTheme::Minimal => minimal_watch_theme(),
    }
}

fn classic_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " tend watch ",
        accent: Color::Cyan,
        accent_fg: Color::Black,
        action: Color::Green,
        action_fg: Color::Black,
        key_bg: Color::Gray,
        key_fg: Color::Black,
        border: Color::DarkGray,
        dim: Color::DarkGray,
        table_header: Color::Gray,
        selected_bg: Color::DarkGray,
        selected_fg: None,
        state_idle: Color::Green,
        state_working: Color::Yellow,
        state_waiting: Color::Yellow,
        state_choice: Color::LightYellow,
        state_error: Color::Red,
        state_starting: Color::Cyan,
        border_type: BorderType::Plain,
    }
}

fn oh_my_tend_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " oh-my-tend ",
        accent: Color::Rgb(177, 139, 255),
        accent_fg: Color::Black,
        action: Color::Rgb(93, 230, 138),
        action_fg: Color::Black,
        key_bg: Color::Rgb(66, 74, 92),
        key_fg: Color::White,
        border: Color::Rgb(94, 234, 212),
        dim: Color::Gray,
        table_header: Color::Rgb(94, 234, 212),
        selected_bg: Color::Rgb(52, 45, 67),
        selected_fg: Some(Color::White),
        state_idle: Color::Rgb(93, 230, 138),
        state_working: Color::Rgb(255, 211, 105),
        state_waiting: Color::Rgb(255, 176, 86),
        state_choice: Color::Rgb(219, 181, 255),
        state_error: Color::Rgb(255, 91, 107),
        state_starting: Color::Rgb(94, 234, 212),
        border_type: BorderType::Rounded,
    }
}

fn focus_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " tend focus ",
        accent: Color::Rgb(125, 211, 252),
        accent_fg: Color::Black,
        action: Color::Rgb(134, 239, 172),
        action_fg: Color::Black,
        key_bg: Color::DarkGray,
        key_fg: Color::White,
        border: Color::DarkGray,
        dim: Color::DarkGray,
        table_header: Color::Rgb(125, 211, 252),
        selected_bg: Color::Rgb(30, 58, 90),
        selected_fg: Some(Color::White),
        state_idle: Color::DarkGray,
        state_working: Color::Rgb(125, 211, 252),
        state_waiting: Color::Yellow,
        state_choice: Color::LightYellow,
        state_error: Color::Red,
        state_starting: Color::Cyan,
        border_type: BorderType::Plain,
    }
}

fn ops_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " tend ops ",
        accent: Color::Yellow,
        accent_fg: Color::Black,
        action: Color::Green,
        action_fg: Color::Black,
        key_bg: Color::Rgb(64, 64, 64),
        key_fg: Color::White,
        border: Color::Yellow,
        dim: Color::Gray,
        table_header: Color::Yellow,
        selected_bg: Color::Rgb(80, 54, 0),
        selected_fg: Some(Color::White),
        state_idle: Color::Green,
        state_working: Color::Cyan,
        state_waiting: Color::LightYellow,
        state_choice: Color::Magenta,
        state_error: Color::LightRed,
        state_starting: Color::LightCyan,
        border_type: BorderType::Plain,
    }
}

fn mono_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " tend mono ",
        accent: Color::White,
        accent_fg: Color::Black,
        action: Color::White,
        action_fg: Color::Black,
        key_bg: Color::DarkGray,
        key_fg: Color::White,
        border: Color::Gray,
        dim: Color::DarkGray,
        table_header: Color::White,
        selected_bg: Color::Gray,
        selected_fg: Some(Color::Black),
        state_idle: Color::Gray,
        state_working: Color::White,
        state_waiting: Color::White,
        state_choice: Color::White,
        state_error: Color::White,
        state_starting: Color::Gray,
        border_type: BorderType::Plain,
    }
}

fn high_contrast_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " tend high-contrast ",
        accent: Color::White,
        accent_fg: Color::Black,
        action: Color::Black,
        action_fg: Color::White,
        key_bg: Color::White,
        key_fg: Color::Black,
        border: Color::White,
        dim: Color::Gray,
        table_header: Color::White,
        selected_bg: Color::White,
        selected_fg: Some(Color::Black),
        state_idle: Color::LightGreen,
        state_working: Color::LightCyan,
        state_waiting: Color::Yellow,
        state_choice: Color::LightMagenta,
        state_error: Color::LightRed,
        state_starting: Color::LightBlue,
        border_type: BorderType::Rounded,
    }
}

fn minimal_watch_theme() -> WatchThemeSpec {
    WatchThemeSpec {
        title: " tend ",
        accent: Color::White,
        accent_fg: Color::Black,
        action: Color::White,
        action_fg: Color::Black,
        key_bg: Color::DarkGray,
        key_fg: Color::White,
        border: Color::DarkGray,
        dim: Color::DarkGray,
        table_header: Color::White,
        selected_bg: Color::Gray,
        selected_fg: Some(Color::Black),
        state_idle: Color::White,
        state_working: Color::White,
        state_waiting: Color::White,
        state_choice: Color::White,
        state_error: Color::White,
        state_starting: Color::White,
        border_type: BorderType::Plain,
    }
}

/// A single column in the watch TUI. The set of valid columns is fixed by
/// this enum — the `[watch]` config picks which ones to show and in what
/// order, but cannot introduce new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchColumn {
    Pane,
    Kind,
    State,
    StateAge,
    Model,
    Ctx,
    Cost,
    Limits,
    Workload,
    Prompt,
    Activity,
    Duration,
}

impl WatchColumn {
    /// Parse a config-string column key. Returns `None` for unknown keys
    /// so the caller can warn and skip rather than refuse to load.
    fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "pane" => Self::Pane,
            "kind" => Self::Kind,
            "state" => Self::State,
            "state_age" => Self::StateAge,
            "model" => Self::Model,
            "ctx" => Self::Ctx,
            "cost" => Self::Cost,
            "limits" => Self::Limits,
            "workload" => Self::Workload,
            "prompt" => Self::Prompt,
            "activity" => Self::Activity,
            "duration" => Self::Duration,
            _ => return None,
        })
    }

    pub(super) fn header(self) -> &'static str {
        match self {
            Self::Pane => "NAME",
            Self::Kind => "KIND",
            Self::State => "ST",
            Self::StateAge => "STATE",
            Self::Model => "MODEL",
            Self::Ctx => "CTX%",
            Self::Cost => "COST$",
            Self::Limits => "LIMITS",
            Self::Workload => "TREE",
            Self::Prompt => "LAST PROMPT",
            Self::Activity => "ACT",
            Self::Duration => "DUR",
        }
    }

    pub(super) fn default_width(self) -> Constraint {
        match self {
            // PANE — "session:window.pane" can run long; 22 covers most.
            Self::Pane => Constraint::Length(22),
            Self::Kind | Self::StateAge => Constraint::Length(12),
            // STATE — compact colored state marker; full state remains
            // available in detail placeholders and structured snapshots.
            Self::State => Constraint::Length(3),
            Self::Model => Constraint::Length(16),
            Self::Ctx => Constraint::Length(5),
            Self::Activity | Self::Duration => Constraint::Length(6),
            Self::Cost => Constraint::Length(7),
            // LIMITS — widest realistic payload is `⛔ 7d in 23h 59m`
            // (~17 cells with a wide-cell emoji). Wider columns crowd the
            // prompt; narrower ones clip the duration suffix on
            // emoji-greedy fonts.
            Self::Limits => Constraint::Length(18),
            Self::Workload => Constraint::Length(8),
            Self::Prompt => Constraint::Min(20),
        }
    }

    /// Build the `Text` content for one cell. Returning `Text` (rather
    /// than a finished `Cell`) lets the caller stack a second line on top
    /// of it when the row is selected and a detail template is enabled.
    pub(super) fn agent_text<'a>(
        self,
        a: &'a Agent,
        now: OffsetDateTime,
        panes: &'a [PaneInfo],
        theme: WatchThemeSpec,
        spin: Spinner,
        summary: WatchSummary,
    ) -> Text<'a> {
        match self {
            Self::Pane => {
                let label = agent_pane_display(a, panes);
                // Dim the pane cell when there's nothing to attach to —
                // a deliberate visual signal that Enter won't do anything
                // useful for this row. Keeping the rest of the row's
                // columns at full brightness preserves readability of
                // state/prompt/etc.
                if a.pane.is_none() {
                    Text::from(Span::styled(
                        label,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM | Modifier::ITALIC),
                    ))
                } else {
                    crate::pane_identity::address_line(
                        label,
                        a.pane.as_deref().unwrap_or(""),
                        a.tmux_socket.as_deref(),
                    )
                    .into()
                }
            }
            Self::Kind => a.kind.to_string().into(),
            Self::State => {
                let (symbol, style) = state_marker(a.state, theme, spin);
                Text::from(Span::styled(symbol, style))
            }
            Self::StateAge => state_age_text(a, now, theme, spin),
            Self::Model => a.model.as_deref().unwrap_or("-").to_string().into(),
            Self::Ctx => a
                .context_used_pct
                .map_or_else(|| "-".into(), |p| format!("{p:>3.0}%"))
                .into(),
            Self::Cost => a
                .cost_usd
                .map_or_else(|| "-".into(), |c| format!("${c:.2}"))
                .into(),
            Self::Limits => limits_text(a, now),
            Self::Workload => workload_text(a),
            Self::Prompt => summary_line(a, summary).into(),
            Self::Activity => relative_time(a.last_activity_at, now).into(),
            Self::Duration => Text::from(Span::styled(
                "-",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )),
        }
    }

    pub(super) fn bare_text(self, p: &PaneInfo) -> Text<'_> {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        match self {
            Self::Pane => {
                let pane = format!("{}:{}.{}", p.session, p.window_index, p.pane_index);
                Text::from(Span::styled(pane, dim))
            }
            // Bare panes have no agent metadata. We surface the pane title /
            // current command in the prompt slot so the row still carries
            // useful at-a-glance info; everything else collapses to a dash.
            Self::Prompt => {
                let summary = if p.title.is_empty() || p.title == p.current_command {
                    p.current_command.clone()
                } else {
                    format!("{}  {}", p.current_command, p.title)
                };
                let summary: String = summary.chars().take(80).collect();
                Text::from(Span::styled(summary, dim))
            }
            Self::Kind | Self::State | Self::StateAge => Text::from(Span::styled("—", dim)),
            Self::Model
            | Self::Ctx
            | Self::Cost
            | Self::Limits
            | Self::Workload
            | Self::Activity
            | Self::Duration => Text::from(Span::styled("-", dim)),
        }
    }

    pub(super) fn work_text<'a>(
        self,
        s: &'a WorkRow,
        now: OffsetDateTime,
        panes: &'a [PaneInfo],
        theme: WatchThemeSpec,
        spin: Spinner,
        summary: WatchSummary,
    ) -> Text<'a> {
        let dim = theme.dim_style().add_modifier(Modifier::DIM);
        if matches!(self, Self::StateAge) {
            return session_state_age_text(s, now, theme, spin);
        }
        let Some(agent) = s.latest_agent.as_ref() else {
            return match self {
                Self::Pane => work_label(s, theme, spin),
                Self::Prompt => Text::from(Span::styled(
                    s.bare_summary.clone().unwrap_or_else(|| {
                        format!("{} pane{}", s.pane_count, plural(s.pane_count))
                    }),
                    dim,
                )),
                Self::Duration => duration_text(s, now),
                Self::Kind | Self::State | Self::StateAge => Text::from(Span::styled("—", dim)),
                Self::Model | Self::Ctx | Self::Cost | Self::Limits | Self::Activity => {
                    Text::from(Span::styled("-", dim))
                }
                Self::Workload => Text::from(Span::styled("-", dim)),
            };
        };

        match self {
            Self::Pane => work_label(s, theme, spin),
            Self::Kind
            | Self::State
            | Self::StateAge
            | Self::Model
            | Self::Ctx
            | Self::Cost
            | Self::Limits
            | Self::Prompt
            | Self::Activity => self.agent_text(agent, now, panes, theme, spin, summary),
            Self::Workload => work_workload_text(s),
            Self::Duration => duration_text(s, now),
        }
    }
}

/// Resolve the configured column list against `WatchConfig`. Unknown keys
/// are skipped with a `tracing::warn!` rather than aborting.
pub(crate) fn resolve_columns(cfg: &WatchConfig) -> Vec<WatchColumn> {
    let mut out = Vec::with_capacity(cfg.columns.len());
    for key in &cfg.columns {
        if let Some(c) = WatchColumn::from_key(key) {
            out.push(c);
        } else {
            tracing::warn!(column = %key, "unknown watch column key, skipping");
        }
    }
    // Warn (once each) on widths entries with no matching column. This
    // catches typos like `widths.prompts = 30` that would otherwise be
    // silently ignored.
    for key in cfg.widths.keys() {
        if WatchColumn::from_key(key).is_none() {
            tracing::warn!(column = %key, "unknown watch.widths key, ignoring");
        }
    }
    out
}

/// Resolve columns and apply the window-view duration default. This is used
/// both at startup and when `:view` changes granularity at runtime so the two
/// paths cannot drift into different table shapes.
pub(super) fn resolve_display_columns(cfg: &WatchConfig) -> Vec<WatchColumn> {
    let mut columns = resolve_columns(cfg);
    if cfg.view == WatchView::Window && !columns.contains(&WatchColumn::Duration) {
        // The default layout carries STATE (glyph + state + age). In window
        // view the leftmost cluster of the WORK cell already shows every
        // state as icon-with-count, so a STATE column is the same fact
        // twice — and 12 columns wide. Swap it for DUR rather than keeping
        // both; DUR + ACT is the whole timing story.
        if columns.as_slice()
            == [
                WatchColumn::Pane,
                WatchColumn::StateAge,
                WatchColumn::Activity,
                WatchColumn::Prompt,
            ]
            || columns.as_slice()
                == [
                    WatchColumn::Pane,
                    WatchColumn::State,
                    WatchColumn::Activity,
                    WatchColumn::Prompt,
                ]
        {
            columns = vec![
                WatchColumn::Pane,
                WatchColumn::Duration,
                WatchColumn::Activity,
                WatchColumn::Prompt,
            ];
        } else {
            let insert_at = columns
                .iter()
                .position(|c| matches!(c, WatchColumn::Prompt | WatchColumn::Activity))
                .unwrap_or(columns.len());
            columns.insert(insert_at, WatchColumn::Duration);
        }
    }
    columns
}

/// Resolve the width Constraint for `col`, falling back to its default
/// when the config has no entry or an `Invalid` spec.
pub(super) fn resolve_width(col: WatchColumn, cfg: &WatchConfig) -> Constraint {
    let key = col.config_key();
    match cfg.widths.get(key) {
        Some(WidthSpec::Length(n)) => Constraint::Length(*n),
        Some(WidthSpec::Min(n)) => Constraint::Min(*n),
        Some(WidthSpec::Percentage(n)) => Constraint::Percentage((*n).min(100)),
        Some(WidthSpec::Invalid(raw)) => {
            tracing::warn!(column = %key, value = %raw, "invalid watch.widths value, using default");
            col.default_width()
        }
        None => col.default_width(),
    }
}

impl WatchColumn {
    fn config_key(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Kind => "kind",
            Self::State => "state",
            Self::StateAge => "state_age",
            Self::Model => "model",
            Self::Ctx => "ctx",
            Self::Cost => "cost",
            Self::Limits => "limits",
            Self::Workload => "workload",
            Self::Prompt => "prompt",
            Self::Activity => "activity",
            Self::Duration => "duration",
        }
    }
}
