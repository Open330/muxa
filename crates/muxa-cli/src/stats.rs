use anyhow::{Context, Result};
use clap::ValueEnum;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::{UTF8_BORDERS_ONLY, UTF8_FULL_CONDENSED};
use comfy_table::{ColumnConstraint, ContentArrangement, Table, Width};
use muxa::event::AgentState;
use muxa::ipc::Client;
use muxa::{
    ActivityEntry, Agent, Config, HistoryEntry, HumanInteractionKind, ScopeExclusions,
    SessionActivity, SessionForegroundEntry, StateTransitionEntry,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use time::{Date, Month, OffsetDateTime, UtcOffset, Weekday};

use crate::theme::{self, CliTheme, TableTone, ThemeArg};
use crate::time_range::TimeRange;
use crate::{terminal_width, truncate_cell, use_colors};

#[derive(Debug, clap::Args)]
// CLI flags are independent toggles, not a state machine to fold into an enum.
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Time window to include: today, yesterday, week, month, last-week, last-month, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "7d")]
    since: String,

    /// Dimension used for the row breakdown.
    #[arg(long, value_enum, default_value_t = GroupBy::Day)]
    group_by: GroupBy,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    /// Render only the WACT time graph. Buckets adapt to the selected range.
    #[arg(long, default_value_t = false)]
    graph: bool,

    /// Shortcut for `--format json`. Overrides `--format`.
    #[arg(long, conflicts_with = "markdown")]
    json: bool,

    /// Shortcut for `--format markdown`. Overrides `--format`.
    #[arg(long)]
    markdown: bool,

    /// Maximum rows to print. Set 0 for all rows.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Column to sort rows by: prompts, work, wait, err, tmux, human, think,
    /// active, block, tok, words, sess, agents, last, or name.
    #[arg(long, value_enum, default_value_t = SortKey::Prompts)]
    sort: SortKey,

    /// Reverse the sort order (numeric columns default to descending, name to ascending).
    #[arg(long, default_value_t = false)]
    reverse: bool,

    /// Exclude pane ids matching a glob. Repeat or comma-separate values.
    #[arg(long = "exclude-pane", value_name = "GLOB", value_delimiter = ',')]
    exclude_pane: Vec<String>,

    /// Exclude tmux session names or ids matching a glob. Repeat or comma-separate values.
    #[arg(long = "exclude-session", value_name = "GLOB", value_delimiter = ',')]
    exclude_session: Vec<String>,

    /// One-shot visual theme override for table output.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,

    /// Print diagnostic table columns plus the full explanatory notes
    /// (methodology for THINK/ACTIVE, the retained-history window, ledger
    /// fallbacks). Without this, the table focuses on ACT/WACT and only
    /// reports that notes exist. JSON/markdown always include all fields.
    #[arg(long, short = 'v', default_value_t = false)]
    verbose: bool,
}

impl Args {
    #[cfg(test)]
    pub(crate) fn theme(&self) -> Option<ThemeArg> {
        self.theme
    }
}

#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// Time window to include: today, yesterday, week, month, last-week, last-month, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "7d")]
    since: String,

    /// Maximum rows per report section. Set 0 for all rows.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Emit JSON instead of the section tables.
    #[arg(long, conflicts_with = "markdown")]
    json: bool,

    /// Emit Markdown instead of the section tables.
    #[arg(long)]
    markdown: bool,

    /// Exclude pane ids matching a glob. Repeat or comma-separate values.
    #[arg(long = "exclude-pane", value_name = "GLOB", value_delimiter = ',')]
    exclude_pane: Vec<String>,

    /// Exclude tmux session names or ids matching a glob. Repeat or comma-separate values.
    #[arg(long = "exclude-session", value_name = "GLOB", value_delimiter = ',')]
    exclude_session: Vec<String>,

    /// One-shot visual theme override for table output.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,

    /// Print diagnostic table columns plus the full explanatory notes
    /// (methodology for THINK/ACTIVE, the retained-history window, ledger
    /// fallbacks). Without this, the tables focus on ACT/WACT and only report
    /// that notes exist. JSON/markdown always include all fields.
    #[arg(long, short = 'v', default_value_t = false)]
    verbose: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Table,
    Json,
    Markdown,
}

impl OutputFormat {
    /// Fold the boolean `--json` / `--markdown` shortcuts onto a base format.
    /// `--json` and `--markdown` are mutually exclusive at the clap layer, so at
    /// most one is set; either one wins over the base, otherwise the base stands.
    fn resolve(base: OutputFormat, json: bool, markdown: bool) -> OutputFormat {
        if json {
            OutputFormat::Json
        } else if markdown {
            OutputFormat::Markdown
        } else {
            base
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum GroupBy {
    Day,
    Project,
    Agent,
    Session,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum SortKey {
    Prompts,
    Work,
    Wait,
    Err,
    Tmux,
    Human,
    Think,
    Active,
    Block,
    Tok,
    Words,
    Sess,
    Agents,
    Last,
    Name,
}

const FULL_STATS_TABLE_WIDTH: usize = 128;
const COMPACT_STATS_TABLE_WIDTH: usize = 76;
const MIN_GROUP_COLUMN_WIDTH: usize = 7;
const MAX_GROUP_COLUMN_WIDTH: usize = 36;

impl GroupBy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Project => "project",
            Self::Agent => "agent",
            Self::Session => "session",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Day => "Day",
            Self::Project => "Project",
            Self::Agent => "Agent",
            Self::Session => "Session",
        }
    }
}

pub async fn run(client: &Client, cfg: &Config, args: Args) -> Result<()> {
    let exclusions = ScopeExclusions::new(args.exclude_pane.clone(), args.exclude_session.clone());
    let data = load_data(client, cfg, &args.since, &exclusions).await?;
    let doc = build_document(
        &data,
        args.group_by,
        args.limit,
        args.sort,
        args.reverse,
        args.graph,
    );
    match OutputFormat::resolve(args.format, args.json, args.markdown) {
        OutputFormat::Table => render_table(
            &doc,
            theme::for_config(cfg, args.theme, use_colors()),
            args.verbose,
        ),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&doc)?),
        OutputFormat::Markdown => print!("{}", render_markdown_stats(&doc)),
    }
    Ok(())
}

pub async fn run_report(client: &Client, cfg: &Config, args: ReportArgs) -> Result<()> {
    let exclusions = ScopeExclusions::new(args.exclude_pane.clone(), args.exclude_session.clone());
    let data = load_data(client, cfg, &args.since, &exclusions).await?;
    let docs = [
        // Days read chronologically (the day-key string sorts by date), so a
        // report shows the shape of the week in order rather than ranked by
        // prompt count. Project/Agent/Session stay ranked by volume.
        build_document(&data, GroupBy::Day, args.limit, SortKey::Name, false, false),
        build_document(
            &data,
            GroupBy::Project,
            args.limit,
            SortKey::Prompts,
            false,
            false,
        ),
        build_document(
            &data,
            GroupBy::Agent,
            args.limit,
            SortKey::Prompts,
            false,
            false,
        ),
        build_document(
            &data,
            GroupBy::Session,
            args.limit,
            SortKey::Prompts,
            false,
            false,
        ),
    ];
    // Report defaults to the same tables as `stats`; `--json`/`--markdown` opt
    // into the machine- and document-friendly formats.
    match OutputFormat::resolve(OutputFormat::Table, args.json, args.markdown) {
        OutputFormat::Table => render_report_tables(
            &docs,
            theme::for_config(cfg, args.theme, use_colors()),
            args.verbose,
        ),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&docs)?),
        OutputFormat::Markdown => print!("{}", render_markdown_report(&docs)),
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct StatsData {
    now: OffsetDateTime,
    range: TimeRange,
    prompts: Vec<HistoryEntry>,
    activity_entries: Vec<ActivityEntry>,
    agents: Vec<Agent>,
    activities: Vec<SessionActivity>,
    pane_sessions: HashMap<String, String>,
    project_by_pane: HashMap<String, String>,
    project_by_agent_session: HashMap<String, String>,
    /// Padding before each action when estimating ACTIVE time (`[stats]` config).
    active_lookback: time::Duration,
    /// Idle timeout after each *prompt* when estimating ACTIVE time.
    active_timeout: time::Duration,
    /// Idle timeout after each *tmux input tick* (keypress / scroll). Shorter than
    /// `active_timeout` so sparse scrolling cannot chain into hours of active time.
    active_tick_timeout: time::Duration,
    /// Whether tmux input ticks seed ACTIVE windows at all (`[stats]` config). When
    /// `false`, keypress/scroll ticks are ignored and ACTIVE anchors only on
    /// submitted prompts and thinking — see `StatsConfig::count_tmux_input`.
    count_tmux_input: bool,
}

#[derive(Debug, Serialize)]
struct StatsDocument {
    generated_at: String,
    /// Raw instant behind `generated_at`, kept for human-facing headers that
    /// render in the viewer's local offset (truncated to whole seconds) so they
    /// reconcile with the local calendar-day rows. Not serialized: JSON keeps
    /// the RFC3339 `generated_at` for machine consumers.
    #[serde(skip)]
    generated_instant: OffsetDateTime,
    range: RangeDocument,
    group_by: String,
    totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_graph: Option<TimeGraph>,
    rows: Vec<GroupRow>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RangeDocument {
    label: String,
    since_at: Option<String>,
    until_at: Option<String>,
    /// Raw instants behind `since_at`/`until_at`, kept for local-offset headers.
    /// Not serialized (see `StatsDocument::generated_instant`).
    #[serde(skip)]
    since_instant: Option<OffsetDateTime>,
    #[serde(skip)]
    until_instant: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
struct Totals {
    prompts: usize,
    prompt_chars: usize,
    words: usize,
    token_estimate: usize,
    agent_sessions: usize,
    live_agents: usize,
    working_secs: u64,
    working: String,
    waiting_secs: u64,
    waiting: String,
    error_secs: u64,
    error: String,
    foreground_secs: u64,
    foreground: String,
    human_secs: u64,
    human: String,
    thinking_secs: u64,
    thinking: String,
    active_secs: u64,
    active: String,
    work_active_secs: u64,
    work_active: String,
    attention_events: usize,
    last_prompt_at: Option<String>,
    last_prompt_age: String,
}

#[derive(Debug, Serialize)]
struct GroupRow {
    key: String,
    prompts: usize,
    prompt_chars: usize,
    words: usize,
    token_estimate: usize,
    agent_sessions: usize,
    live_agents: usize,
    working_secs: u64,
    working: String,
    waiting_secs: u64,
    waiting: String,
    error_secs: u64,
    error: String,
    foreground_secs: u64,
    foreground: String,
    human_secs: u64,
    human: String,
    thinking_secs: u64,
    thinking: String,
    active_secs: u64,
    active: String,
    work_active_secs: u64,
    work_active: String,
    attention_events: usize,
    last_prompt_at: Option<String>,
    last_prompt_age: String,
}

#[derive(Debug, Serialize)]
struct TimeGraph {
    metric: String,
    bucket: String,
    total_secs: u64,
    total: String,
    max_secs: u64,
    max: String,
    buckets: Vec<TimeGraphBucket>,
}

#[derive(Debug, Serialize)]
struct TimeGraphBucket {
    label: String,
    started_at: String,
    ended_at: String,
    active_secs: u64,
    active: String,
    work_active_secs: u64,
    work_active: String,
}

#[derive(Default)]
struct GroupAccumulator {
    prompts: usize,
    prompt_chars: usize,
    words: usize,
    token_estimate: usize,
    agent_sessions: BTreeSet<String>,
    live_agents: usize,
    working_secs: u64,
    waiting_secs: u64,
    error_secs: u64,
    foreground_secs: u64,
    human_secs: u64,
    thinking_secs: u64,
    active_secs: u64,
    work_active_secs: u64,
    attention_events: usize,
    last_prompt_at: Option<OffsetDateTime>,
}

impl GroupAccumulator {
    /// True when this bucket carries no information at all — no prompts, no
    /// tracked time, no attention events, no live agents. Seeders (open
    /// sessions, live-agent state, activity ledger) can insert such empty
    /// buckets for a day with no real activity, which surfaced as phantom
    /// all-`-` day rows for dates outside the requested window.
    fn is_empty(&self) -> bool {
        self.prompts == 0
            && self.live_agents == 0
            && self.attention_events == 0
            && self.working_secs == 0
            && self.waiting_secs == 0
            && self.error_secs == 0
            && self.foreground_secs == 0
            && self.human_secs == 0
            && self.thinking_secs == 0
            && self.active_secs == 0
            && self.work_active_secs == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ActiveDuration {
    pub active_secs: u64,
    pub work_active_secs: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionActiveStats {
    pub totals: ActiveDuration,
    pub by_session: BTreeMap<String, ActiveDuration>,
}

#[derive(Debug, Clone)]
struct ScopedInterval {
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
    pane: Option<String>,
    session_name: Option<String>,
    scope_key: String,
}

#[derive(Debug, Clone)]
struct AttentionInterval {
    interval: ScopedInterval,
    group_key: String,
    work_eligible: bool,
    /// Recency time for last-touch attribution — the *unclipped* action moment
    /// (prompt / tmux input / thinking start). Kept separate from
    /// `interval.started_at`, which the range may clamp to the window boundary
    /// (collapsing distinct starts to a tie); the anchor preserves true order.
    anchor: OffsetDateTime,
}

async fn load_data(
    client: &Client,
    cfg: &Config,
    since: &str,
    exclusions: &ScopeExclusions,
) -> Result<StatsData> {
    let now = OffsetDateTime::now_utc();
    let range = parse_since(since, now)?;

    let mut prompts = client
        .recent_prompts(None, Some(0))
        .await
        .context("querying daemon prompt history")?;
    prompts.retain(|entry| range.includes(entry.at));

    let activity_entries = load_activity_entries(cfg).await;
    let agents = client
        .snapshot()
        .await
        .context("querying daemon agent snapshot")?;
    let activities = load_session_activities(cfg).await;
    let panes = muxa::default_backend().list_panes();

    let pane_sessions = panes
        .iter()
        .map(|pane| (pane.pane_id.clone(), pane.session.clone()))
        .collect::<HashMap<_, _>>();

    let mut project_by_pane = HashMap::new();
    let mut project_by_agent_session = HashMap::new();
    for agent in &agents {
        let Some(project) = project_from_cwd(agent.cwd.as_deref()) else {
            continue;
        };
        project_by_agent_session
            .entry(agent.session_id.clone())
            .or_insert_with(|| project.clone());
        if let Some(pane) = agent.pane.as_ref() {
            project_by_pane
                .entry(pane.clone())
                .or_insert_with(|| project.clone());
        }
    }

    let mut data = StatsData {
        now,
        range,
        prompts,
        activity_entries,
        agents,
        activities,
        pane_sessions,
        project_by_pane,
        project_by_agent_session,
        active_lookback: secs_to_duration(cfg.stats.active_lookback_secs),
        active_timeout: secs_to_duration(cfg.stats.active_timeout_secs),
        active_tick_timeout: secs_to_duration(cfg.stats.active_tick_timeout_secs),
        count_tmux_input: cfg.stats.count_tmux_input,
    };
    apply_exclusions(&mut data, exclusions);
    Ok(data)
}

pub(crate) async fn session_active_stats(
    client: &Client,
    cfg: &Config,
    since: &str,
    exclusions: &ScopeExclusions,
) -> Result<SessionActiveStats> {
    let data = load_data(client, cfg, since, exclusions).await?;
    let attribution = last_touch_attribution(&active_windows(&data, GroupBy::Session));
    let mut by_session = BTreeMap::<String, ActiveDuration>::new();

    for (session, secs) in attribution.active {
        by_session.entry(session).or_default().active_secs = secs;
    }
    for (session, secs) in attribution.work_active {
        by_session.entry(session).or_default().work_active_secs = secs;
    }

    let totals = by_session
        .values()
        .fold(ActiveDuration::default(), |mut total, row| {
            total.active_secs = total.active_secs.saturating_add(row.active_secs);
            total.work_active_secs = total.work_active_secs.saturating_add(row.work_active_secs);
            total
        });

    Ok(SessionActiveStats { totals, by_session })
}

/// Convert a config `u64` seconds value into a `time::Duration`, clamping the
/// `> i64::MAX` case so the conversion itself can't panic. (A merely absurd
/// multi-millennium value can still overflow later date arithmetic, but no sane
/// `[stats]` setting reaches that.)
fn secs_to_duration(secs: u64) -> time::Duration {
    time::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX))
}

fn apply_exclusions(data: &mut StatsData, exclusions: &ScopeExclusions) {
    if exclusions.is_empty() {
        return;
    }

    let pane_sessions = data.pane_sessions.clone();
    data.prompts
        .retain(|prompt| !prompt_excluded(exclusions, prompt, &pane_sessions));
    data.activity_entries
        .retain(|entry| !activity_entry_excluded(exclusions, entry, &pane_sessions));
    data.agents
        .retain(|agent| !agent_excluded(exclusions, agent, &pane_sessions));
    data.activities
        .retain(|activity| !session_activity_excluded(exclusions, activity));
    data.project_by_pane.retain(|pane, _| {
        !exclusions.excludes(
            Some(pane),
            None,
            pane_sessions.get(pane).map(String::as_str),
        )
    });
    data.project_by_agent_session
        .retain(|session_id, _| !exclusions.excludes(None, Some(session_id), None));
}

fn prompt_excluded(
    exclusions: &ScopeExclusions,
    prompt: &HistoryEntry,
    pane_sessions: &HashMap<String, String>,
) -> bool {
    let session_name = prompt
        .tmux_session
        .as_deref()
        .or_else(|| pane_sessions.get(&prompt.pane).map(String::as_str));
    exclusions.excludes(
        Some(prompt.pane.as_str()),
        Some(prompt.session_id.as_str()),
        session_name,
    )
}

fn activity_entry_excluded(
    exclusions: &ScopeExclusions,
    entry: &ActivityEntry,
    pane_sessions: &HashMap<String, String>,
) -> bool {
    match entry {
        ActivityEntry::StateTransition(entry) => {
            let session_name = entry.session_name.as_deref().or_else(|| {
                entry
                    .pane
                    .as_ref()
                    .and_then(|pane| pane_sessions.get(pane))
                    .map(String::as_str)
            });
            exclusions.excludes(entry.pane.as_deref(), Some(&entry.session_id), session_name)
        }
        ActivityEntry::SessionForeground(entry) => {
            exclusions.excludes(None, Some(&entry.session_id), Some(&entry.session_name))
        }
        ActivityEntry::HumanInteraction(entry) => exclusions.excludes(
            entry.pane.as_deref(),
            entry.session_id.as_deref(),
            entry.session_name.as_deref(),
        ),
    }
}

fn agent_excluded(
    exclusions: &ScopeExclusions,
    agent: &Agent,
    pane_sessions: &HashMap<String, String>,
) -> bool {
    let session_name = agent
        .pane
        .as_ref()
        .and_then(|pane| pane_sessions.get(pane))
        .map(String::as_str);
    exclusions.excludes(agent.pane.as_deref(), Some(&agent.session_id), session_name)
}

fn session_activity_excluded(exclusions: &ScopeExclusions, activity: &SessionActivity) -> bool {
    exclusions.excludes(None, Some(&activity.session_id), Some(&activity.name))
}

async fn load_activity_entries(cfg: &Config) -> Vec<ActivityEntry> {
    if !cfg.activity.enabled {
        return Vec::new();
    }
    let Some(path) = cfg
        .activity
        .path
        .clone()
        .or_else(muxa::paths::default_activity_file)
    else {
        return Vec::new();
    };
    muxa::activity::load(&path).await.unwrap_or_default()
}

async fn load_session_activities(cfg: &Config) -> Vec<SessionActivity> {
    if !cfg.session_activity.enabled {
        return Vec::new();
    }
    let Some(path) = cfg
        .session_activity
        .path
        .clone()
        .or_else(muxa::paths::default_session_activity_file)
    else {
        return Vec::new();
    };
    muxa::session_activity::load(&path).await
}

fn build_document(
    data: &StatsData,
    group_by: GroupBy,
    limit: usize,
    sort: SortKey,
    reverse: bool,
    include_graph: bool,
) -> StatsDocument {
    let rows = build_rows(data, group_by, limit, sort, reverse);
    StatsDocument {
        generated_at: format_rfc3339(data.now),
        generated_instant: data.now,
        range: RangeDocument {
            label: data.range.label.clone(),
            since_at: data.range.since_at.map(format_rfc3339),
            until_at: data.range.until_at.map(format_rfc3339),
            since_instant: data.range.since_at,
            until_instant: data.range.until_at,
        },
        group_by: group_by.as_str().to_string(),
        totals: build_totals(data),
        time_graph: include_graph.then(|| build_time_graph(data)).flatten(),
        rows,
        notes: notes(data),
    }
}

fn build_totals(data: &StatsData) -> Totals {
    let mut agent_sessions = BTreeSet::new();
    let mut prompt_chars = 0usize;
    let mut words = 0usize;
    let mut token_estimate = 0usize;
    let mut last_prompt_at = None;
    let mut working_secs = 0u64;
    let mut waiting_secs = 0u64;
    let mut error_secs = 0u64;
    let mut foreground_secs = 0u64;
    let mut human_secs;
    let mut attention_events = 0usize;
    let mut has_session_foreground_ledger = false;

    for prompt in &data.prompts {
        let metrics = prompt_metrics(prompt);
        prompt_chars += metrics.chars;
        words += metrics.words;
        token_estimate += metrics.token_estimate;
        agent_sessions.insert(prompt.session_id.clone());
        update_max_time(&mut last_prompt_at, prompt.at);
    }

    for entry in &data.activity_entries {
        match entry {
            ActivityEntry::StateTransition(entry) => {
                let secs = state_transition_overlap_secs(data, entry);
                add_state_secs(
                    entry.from,
                    secs,
                    &mut working_secs,
                    &mut waiting_secs,
                    &mut error_secs,
                );
                if data.range.includes(entry.at) && is_attention_state(entry.to) {
                    attention_events += 1;
                }
            }
            ActivityEntry::SessionForeground(entry) => {
                has_session_foreground_ledger = true;
                foreground_secs += session_foreground_overlap_secs(data, entry);
            }
            ActivityEntry::HumanInteraction(_) => {}
        }
    }
    foreground_secs += open_session_foreground_secs(data);
    if !has_session_foreground_ledger {
        foreground_secs = foreground_secs.saturating_add(legacy_foreground_secs(data));
    }
    for agent in &data.agents {
        let secs = open_agent_state_secs(data, agent);
        add_state_secs(
            agent.state,
            secs,
            &mut working_secs,
            &mut waiting_secs,
            &mut error_secs,
        );
    }
    human_secs =
        sum_merged_scoped_intervals(&human_presence_intervals(data, HumanPresenceMode::Human));
    if !has_session_foreground_ledger {
        human_secs = human_secs.saturating_add(legacy_foreground_secs(data));
    }
    let thinking_secs = thinking_secs_total(data);
    let active_secs = active_secs_total(data);
    let work_active_secs = work_active_secs_total(data);

    Totals {
        prompts: data.prompts.len(),
        prompt_chars,
        words,
        token_estimate,
        agent_sessions: agent_sessions.len(),
        live_agents: data
            .agents
            .iter()
            .filter(|agent| agent.state != AgentState::Stopped)
            .count(),
        working_secs,
        working: format_duration(working_secs),
        waiting_secs,
        waiting: format_duration(waiting_secs),
        error_secs,
        error: format_duration(error_secs),
        foreground_secs,
        foreground: format_duration(foreground_secs),
        human_secs,
        human: format_duration(human_secs),
        thinking_secs,
        thinking: format_duration(thinking_secs),
        active_secs,
        active: format_duration(active_secs),
        work_active_secs,
        work_active: format_duration(work_active_secs),
        attention_events,
        last_prompt_at: last_prompt_at.map(format_rfc3339),
        last_prompt_age: last_prompt_at
            .map_or_else(|| "-".to_string(), |at| relative_time(data.now, at)),
    }
}

fn build_rows(
    data: &StatsData,
    group_by: GroupBy,
    limit: usize,
    sort: SortKey,
    reverse: bool,
) -> Vec<GroupRow> {
    let mut rows = BTreeMap::<String, GroupAccumulator>::new();
    let session_foreground_ledger = has_session_foreground_ledger(data);

    for prompt in &data.prompts {
        let key = prompt_group_key(data, prompt, group_by);
        let metrics = prompt_metrics(prompt);
        let acc = rows.entry(key).or_default();
        acc.prompts += 1;
        acc.prompt_chars += metrics.chars;
        acc.words += metrics.words;
        acc.token_estimate += metrics.token_estimate;
        acc.agent_sessions.insert(prompt.session_id.clone());
        update_max_time(&mut acc.last_prompt_at, prompt.at);
    }

    add_activity_rows(data, group_by, &mut rows);
    add_human_rows(data, group_by, &mut rows);
    add_thinking_rows(data, group_by, &mut rows);
    add_active_rows(data, group_by, &mut rows);
    add_work_active_rows(data, group_by, &mut rows);

    if group_by != GroupBy::Session || session_foreground_ledger {
        add_open_session_foreground_rows(data, group_by, &mut rows);
    }

    for agent in &data.agents {
        let secs = open_agent_state_secs(data, agent);
        if secs == 0 {
            continue;
        }
        let key = agent_group_key(data, agent, group_by);
        let acc = rows.entry(key).or_default();
        add_state_secs(
            agent.state,
            secs,
            &mut acc.working_secs,
            &mut acc.waiting_secs,
            &mut acc.error_secs,
        );
    }

    for agent in &data.agents {
        if agent.state == AgentState::Stopped || !data.range.includes(agent.last_activity_at) {
            continue;
        }
        let key = agent_group_key(data, agent, group_by);
        rows.entry(key).or_default().live_agents += 1;
    }

    if group_by == GroupBy::Session && !session_foreground_ledger {
        for activity in &data.activities {
            let key = activity.name.clone();
            let secs = activity.effective_total_secs(data.now);
            let acc = rows.entry(key).or_default();
            acc.foreground_secs += secs;
            acc.human_secs += secs;
        }
    }

    let mut rows = rows.into_iter().collect::<Vec<_>>();
    if group_by == GroupBy::Day {
        // Drop phantom all-empty day buckets: seeding can insert a key for a
        // day with no measured activity, which showed up as all-`-` rows for
        // dates well outside the requested window (e.g. a "2026-05-11" row in a
        // `--since today` view).
        rows.retain(|(_, acc)| !acc.is_empty());
    }
    sort_group_rows(&mut rows, sort, reverse);
    if limit > 0 {
        rows.truncate(limit);
    }

    rows.into_iter()
        .map(|(key, acc)| GroupRow {
            key,
            prompts: acc.prompts,
            prompt_chars: acc.prompt_chars,
            words: acc.words,
            token_estimate: acc.token_estimate,
            agent_sessions: acc.agent_sessions.len(),
            live_agents: acc.live_agents,
            working_secs: acc.working_secs,
            working: format_duration(acc.working_secs),
            waiting_secs: acc.waiting_secs,
            waiting: format_duration(acc.waiting_secs),
            error_secs: acc.error_secs,
            error: format_duration(acc.error_secs),
            foreground_secs: acc.foreground_secs,
            foreground: format_duration(acc.foreground_secs),
            human_secs: acc.human_secs,
            human: format_duration(acc.human_secs),
            thinking_secs: acc.thinking_secs,
            thinking: format_duration(acc.thinking_secs),
            active_secs: acc.active_secs,
            active: format_duration(acc.active_secs),
            work_active_secs: acc.work_active_secs,
            work_active: format_duration(acc.work_active_secs),
            attention_events: acc.attention_events,
            last_prompt_at: acc.last_prompt_at.map(format_rfc3339),
            last_prompt_age: acc
                .last_prompt_at
                .map_or_else(|| "-".to_string(), |at| relative_time(data.now, at)),
        })
        .collect()
}

fn sort_group_rows(rows: &mut [(String, GroupAccumulator)], sort: SortKey, reverse: bool) {
    rows.sort_by(|(a_key, a), (b_key, b)| {
        // Numeric columns default to descending (largest first); `name` defaults
        // to ascending. `--reverse` flips whichever default the column carries.
        let ordering = match sort {
            SortKey::Prompts => b
                .prompts
                .cmp(&a.prompts)
                .then_with(|| b.foreground_secs.cmp(&a.foreground_secs))
                .then_with(|| b.last_prompt_at.cmp(&a.last_prompt_at)),
            SortKey::Work => b.working_secs.cmp(&a.working_secs),
            SortKey::Wait => b.waiting_secs.cmp(&a.waiting_secs),
            SortKey::Err => b.error_secs.cmp(&a.error_secs),
            SortKey::Tmux => b.foreground_secs.cmp(&a.foreground_secs),
            SortKey::Human => b.human_secs.cmp(&a.human_secs),
            SortKey::Think => b.thinking_secs.cmp(&a.thinking_secs),
            SortKey::Active => b.active_secs.cmp(&a.active_secs),
            SortKey::Block => b.attention_events.cmp(&a.attention_events),
            SortKey::Tok => b.token_estimate.cmp(&a.token_estimate),
            SortKey::Words => b.words.cmp(&a.words),
            SortKey::Sess => b.agent_sessions.len().cmp(&a.agent_sessions.len()),
            SortKey::Agents => b.live_agents.cmp(&a.live_agents),
            SortKey::Last => b.last_prompt_at.cmp(&a.last_prompt_at),
            SortKey::Name => a_key.cmp(b_key),
        };
        let ordering = if reverse {
            ordering.reverse()
        } else {
            ordering
        };
        // Stable, deterministic tie-break so equal rows keep a fixed order.
        ordering.then_with(|| a_key.cmp(b_key))
    });
}

fn add_activity_rows(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
) {
    for entry in &data.activity_entries {
        match entry {
            ActivityEntry::StateTransition(entry) => add_state_transition_row(
                data,
                group_by,
                rows,
                entry,
                state_transition_overlap_secs(data, entry),
            ),
            ActivityEntry::SessionForeground(entry) => {
                let secs = session_foreground_overlap_secs(data, entry);
                if secs == 0 {
                    continue;
                }
                if let Some(key) = session_foreground_group_key(entry, group_by) {
                    rows.entry(key).or_default().foreground_secs += secs;
                }
            }
            ActivityEntry::HumanInteraction(_) => {}
        }
    }
}

fn add_human_rows(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
) {
    let mut grouped: BTreeMap<String, Vec<ScopedInterval>> = BTreeMap::new();
    for interval in human_presence_intervals(data, HumanPresenceMode::Human) {
        if let Some(key) = human_presence_group_key(data, &interval, group_by) {
            grouped.entry(key).or_default().push(interval);
        }
    }
    for (key, intervals) in grouped {
        let secs = sum_merged_scoped_intervals(&intervals);
        if secs > 0 {
            rows.entry(key).or_default().human_secs += secs;
        }
    }
}

fn add_thinking_rows(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
) {
    let presences = human_presence_intervals(data, HumanPresenceMode::Thinking);
    let mut grouped: BTreeMap<String, Vec<ScopedInterval>> = BTreeMap::new();
    for attention in attention_intervals(data, group_by) {
        for segment in overlapping_presence_segments(&attention.interval, &presences) {
            grouped
                .entry(attention.group_key.clone())
                .or_default()
                .push(segment);
        }
    }
    for (key, intervals) in grouped {
        let secs = sum_merged_scoped_intervals(&intervals);
        if secs > 0 {
            rows.entry(key).or_default().thinking_secs += secs;
        }
    }
}

/// Estimate active human time as the union of three signals, each clipped to
/// observed human presence so padded windows cannot outlive the session
/// foreground/interaction that made them plausible:
///
/// 1. **Submitted prompts** — padded `active_lookback` before / `active_timeout`
///    after each prompt (both from `[stats]` config). The unambiguous "I typed
///    something" action.
/// 2. **tmux input ticks** — the daemon records one whenever a client's
///    `client_activity` advances. A `TmuxInput` (keypress) tick is work-eligible;
///    a `TmuxScroll` (scrollback) tick is engaged-only. Each is padded
///    `active_lookback` before / `active_tick_timeout` after — a shorter timeout
///    than prompts, so sparse scrolling can't chain into hours.
/// 3. **Thinking** — time spent present while an agent is blocked on you
///    (`WaitingInput`/`WaitingChoice`/`Error`), i.e. reading its question and
///    deciding even without a keystroke.
///
/// A pane left attached with no prompts, no input, and no waiting agent yields
/// none of the three, which is what keeps a forgotten attach from ballooning the
/// estimate to hours. Prompt/input padding is a confidence window, not extra
/// presence: if no matching human presence was observed, it contributes nothing.
fn anchor_intervals(data: &StatsData, group_by: GroupBy) -> Vec<AttentionInterval> {
    let mut intervals = Vec::new();
    let active_presences = human_presence_intervals(data, HumanPresenceMode::Active);

    // (1) Submitted prompts. `data.prompts` is already clipped to the range, so
    // a prompt just outside a bounded `--since` whose padded window would poke
    // into the range is not credited — a bounded-edge undercount of at most
    // `active_timeout` per session, accepted to keep prompt counts range-exact.
    for prompt in &data.prompts {
        let session_name = prompt
            .tmux_session
            .clone()
            .or_else(|| data.pane_sessions.get(&prompt.pane).cloned());
        if let Some(interval) = scoped_interval(
            data,
            prompt.at - data.active_lookback,
            prompt.at + data.active_timeout,
            Some(prompt.pane.clone()),
            session_name,
            &prompt.session_id,
        ) {
            let group_key = prompt_group_key(data, prompt, group_by);
            for segment in overlapping_presence_segments(&interval, &active_presences) {
                intervals.push(AttentionInterval {
                    interval: segment,
                    group_key: group_key.clone(),
                    work_eligible: true,
                    anchor: prompt.at,
                });
            }
        }
    }

    // (2) tmux input ticks (keypress / scroll while attached). Skipped entirely
    // when `count_tmux_input` is off: tmux can't distinguish a keypress from mouse
    // motion/wheel/focus behind `#{client_activity}`, so with `mouse on` these
    // ticks credit ACTIVE to a session the human only hovered over. Disabling them
    // leaves prompts and thinking — deliberate actions — as the only ACT anchors.
    for entry in data
        .count_tmux_input
        .then_some(&data.activity_entries)
        .into_iter()
        .flatten()
    {
        let ActivityEntry::HumanInteraction(entry) = entry else {
            continue;
        };
        if !entry.kind.is_input_tick() {
            continue;
        }
        // Only ticks whose own time is in range count, mirroring prompts (which
        // `load_data` pre-filters by `prompt.at`). Otherwise an out-of-range tick
        // whose padded window merely overlaps the range would create a row for a
        // day outside the request and mis-bucket its clipped seconds.
        if !data.range.includes(entry.ended_at) {
            continue;
        }
        if let Some(interval) = scoped_interval(
            data,
            entry.started_at - data.active_lookback,
            entry.ended_at + data.active_tick_timeout,
            entry.pane.clone(),
            entry.session_name.clone(),
            entry.session_id.as_deref().unwrap_or("human_interaction"),
        ) {
            // Bucket by the tick's own time, not the padded window end — a tick
            // near the end of a day must land on that day (like prompts keyed by
            // `prompt.at`), not roll into the next via `+active_tick_timeout`.
            let group_key = match group_by {
                GroupBy::Day => Some(format_day(entry.ended_at)),
                _ => human_presence_group_key(data, &interval, group_by),
            };
            if let Some(group_key) = group_key {
                for segment in overlapping_presence_segments(&interval, &active_presences) {
                    intervals.push(AttentionInterval {
                        interval: segment,
                        group_key: group_key.clone(),
                        work_eligible: entry.kind.is_work_input(),
                        anchor: entry.ended_at,
                    });
                }
            }
        }
    }

    // (3) Thinking: present while an agent is blocked on you (reading/deciding).
    let presences = human_presence_intervals(data, HumanPresenceMode::Thinking);
    for attention in attention_intervals(data, group_by) {
        for segment in overlapping_presence_segments(&attention.interval, &presences) {
            let anchor = segment.started_at;
            intervals.push(AttentionInterval {
                interval: segment,
                group_key: attention.group_key.clone(),
                work_eligible: true,
                anchor,
            });
        }
    }

    intervals
}

/// One active window for the last-touch sweep: the (possibly range-clipped)
/// `[start, end)` span in whole unix seconds, plus the *unclipped* `anchor`
/// (nanoseconds, for ranking recency only) and the group key. The span is whole
/// seconds — one truncation per endpoint — so the per-slice sweep and the grand
/// total agree exactly; the anchor keeps nanosecond precision so two touches in
/// the same second still order correctly.
struct ActiveWindow {
    start: i64,
    end: i64,
    anchor: i128,
    group_key: String,
    work_eligible: bool,
}

fn active_windows(data: &StatsData, group_by: GroupBy) -> Vec<ActiveWindow> {
    anchor_intervals(data, group_by)
        .into_iter()
        .map(|a| ActiveWindow {
            start: a.interval.started_at.unix_timestamp(),
            end: a.interval.ended_at.unix_timestamp(),
            anchor: a.anchor.unix_timestamp_nanos(),
            group_key: a.group_key,
            work_eligible: a.work_eligible,
        })
        .collect()
}

/// Grand-total ACTIVE: the wall-clock during which the human was engaged with
/// *any* session. A human does one thing at a time, so this is the union of all
/// active windows (overlaps counted once) — it does not multiply elapsed time
/// when several agents are juggled at once. Equals the sum of the per-session
/// `add_active_rows` shares (both run the same last-touch sweep).
fn active_secs_total(data: &StatsData) -> u64 {
    last_touch_attribution(&active_windows(data, GroupBy::Session))
        .active
        .values()
        .sum()
}

/// Grand-total `WORK_ACTIVE`: the subset of [`active_secs_total`] whose winning
/// last-touch window is work-eligible (prompt, keypress, or thinking). Scrollback
/// ticks still participate in ACT attribution, but their slices do not count as
/// hands-on work.
fn work_active_secs_total(data: &StatsData) -> u64 {
    last_touch_attribution(&active_windows(data, GroupBy::Session))
        .work_active
        .values()
        .sum()
}

/// Per-group ACTIVE with cross-session de-duplication: every wall-clock instant
/// is attributed to a single group via "last touch" — the active window with the
/// most recent start covering it (whatever the human most recently acted on).
/// The per-group values therefore sum to the de-duplicated total rather than
/// over-counting overlapping windows from concurrent sessions.
fn add_active_rows(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
) {
    for (key, secs) in last_touch_attribution(&active_windows(data, group_by)).active {
        if secs > 0 {
            rows.entry(key).or_default().active_secs += secs;
        }
    }
}

/// Per-group `WORK_ACTIVE`: the work-eligible subset of the same last-touch
/// attribution used for ACT, so every row's WACT stays within that row's ACT.
fn add_work_active_rows(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
) {
    for (key, secs) in last_touch_attribution(&active_windows(data, group_by)).work_active {
        if secs > 0 {
            rows.entry(key).or_default().work_active_secs += secs;
        }
    }
}

/// Attribute each second of the union of `windows` to exactly one group key: the
/// covering window with the latest `anchor` ("last touch" — whatever the human
/// most recently acted on). Recency uses the unclipped `anchor`, not the span
/// start, so windows clamped to the same range boundary still rank correctly.
/// Returns active and work-active seconds per group key; active values sum to
/// the union of all windows (each second counted once), so a grand total taken
/// as `values().sum()` never exceeds elapsed time. Work-active is a subset of
/// that same attribution, which keeps WACT within ACT for every row.
#[derive(Default)]
struct ActiveAttribution {
    active: BTreeMap<String, u64>,
    work_active: BTreeMap<String, u64>,
}

fn last_touch_attribution(windows: &[ActiveWindow]) -> ActiveAttribution {
    // (time, is_end, window index). All events at a given instant are applied
    // before the following slice is measured, so a window ending exactly as
    // another starts hands off cleanly.
    let mut events: Vec<(i64, bool, usize)> = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        if w.end > w.start {
            events.push((w.start, false, i));
            events.push((w.end, true, i));
        }
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    // Active windows as (anchor_nanos, index); the max element is the latest touch.
    let mut active: BTreeSet<(i128, usize)> = BTreeSet::new();
    let mut out = ActiveAttribution::default();
    let mut i = 0;
    while i < events.len() {
        let t = events[i].0;
        while i < events.len() && events[i].0 == t {
            let (_, is_end, idx) = events[i];
            let key = (windows[idx].anchor, idx);
            if is_end {
                active.remove(&key);
            } else {
                active.insert(key);
            }
            i += 1;
        }
        if i < events.len() {
            let secs = u64::try_from(events[i].0 - t).unwrap_or(0);
            if secs > 0 {
                if let Some(&(_, idx)) = active.iter().next_back() {
                    *out.active
                        .entry(windows[idx].group_key.clone())
                        .or_default() += secs;
                    if windows[idx].work_eligible {
                        *out.work_active
                            .entry(windows[idx].group_key.clone())
                            .or_default() += secs;
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeBucketKind {
    Hour,
    Day,
    Week,
    Month,
}

impl TimeBucketKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

#[derive(Debug)]
struct TimeBucketAccumulator {
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
    label: String,
    active_secs: u64,
    work_active_secs: u64,
}

fn build_time_graph(data: &StatsData) -> Option<TimeGraph> {
    let windows = active_windows(data, GroupBy::Session);
    let (window_start, window_end) = time_graph_window(data, &windows)?;
    let kind = choose_time_bucket(window_start, window_end);
    let offset = local_offset();
    let mut buckets = time_bucket_accumulators(window_start, window_end, kind, offset);
    if buckets.is_empty() {
        return None;
    }

    add_active_windows_to_time_buckets(&mut buckets, &windows);

    let total_secs = buckets
        .iter()
        .map(|bucket| bucket.work_active_secs)
        .sum::<u64>();
    let max_secs = buckets
        .iter()
        .map(|bucket| bucket.work_active_secs)
        .max()
        .unwrap_or(0);
    Some(TimeGraph {
        metric: "work_active".to_string(),
        bucket: kind.as_str().to_string(),
        total_secs,
        total: format_duration(total_secs),
        max_secs,
        max: format_duration(max_secs),
        buckets: buckets
            .into_iter()
            .map(|bucket| TimeGraphBucket {
                label: bucket.label,
                started_at: format_rfc3339(bucket.started_at),
                ended_at: format_rfc3339(bucket.ended_at),
                active_secs: bucket.active_secs,
                active: format_duration(bucket.active_secs),
                work_active_secs: bucket.work_active_secs,
                work_active: format_duration(bucket.work_active_secs),
            })
            .collect(),
    })
}

fn time_graph_window(
    data: &StatsData,
    windows: &[ActiveWindow],
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let end = data.range.effective_end(data.now);
    let start = if let Some(since_at) = data.range.since_at {
        since_at
    } else {
        let first_window = windows
            .iter()
            .filter(|window| window.end > window.start)
            .map(|window| window.start)
            .min()?;
        OffsetDateTime::from_unix_timestamp(first_window).ok()?
    };
    (end > start).then_some((start, end))
}

fn choose_time_bucket(start: OffsetDateTime, end: OffsetDateTime) -> TimeBucketKind {
    let secs = (end - start).whole_seconds().max(0);
    if secs <= 2 * 24 * 60 * 60 {
        TimeBucketKind::Hour
    } else if secs <= 60 * 24 * 60 * 60 {
        TimeBucketKind::Day
    } else if secs <= 400 * 24 * 60 * 60 {
        TimeBucketKind::Week
    } else {
        TimeBucketKind::Month
    }
}

fn time_bucket_accumulators(
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
    kind: TimeBucketKind,
    offset: UtcOffset,
) -> Vec<TimeBucketAccumulator> {
    let mut buckets = Vec::new();
    let mut cursor = floor_time_bucket(window_start, kind, offset);
    let end_anchor = (window_end - time::Duration::seconds(1)).max(window_start);
    let hour_labels_include_date = kind == TimeBucketKind::Hour
        && window_start.to_offset(offset).date() != end_anchor.to_offset(offset).date();
    while cursor < window_end {
        let next = advance_time_bucket(cursor, kind, offset);
        if next <= cursor {
            break;
        }
        buckets.push(TimeBucketAccumulator {
            started_at: cursor,
            ended_at: next,
            label: time_bucket_label(cursor, kind, offset, hour_labels_include_date),
            active_secs: 0,
            work_active_secs: 0,
        });
        cursor = next;
    }
    buckets
}

fn add_active_windows_to_time_buckets(
    buckets: &mut [TimeBucketAccumulator],
    windows: &[ActiveWindow],
) {
    let mut events: Vec<(i64, bool, usize)> = Vec::new();
    for (i, window) in windows.iter().enumerate() {
        if window.end > window.start {
            events.push((window.start, false, i));
            events.push((window.end, true, i));
        }
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut active: BTreeSet<(i128, usize)> = BTreeSet::new();
    let mut i = 0;
    while i < events.len() {
        let t = events[i].0;
        while i < events.len() && events[i].0 == t {
            let (_, is_end, idx) = events[i];
            let key = (windows[idx].anchor, idx);
            if is_end {
                active.remove(&key);
            } else {
                active.insert(key);
            }
            i += 1;
        }
        if i < events.len() {
            let next = events[i].0;
            if next > t {
                if let Some(&(_, idx)) = active.iter().next_back() {
                    add_active_slice_to_time_buckets(buckets, t, next, windows[idx].work_eligible);
                }
            }
        }
    }
}

fn add_active_slice_to_time_buckets(
    buckets: &mut [TimeBucketAccumulator],
    started_at: i64,
    ended_at: i64,
    work_eligible: bool,
) {
    for bucket in buckets {
        let bucket_start = bucket.started_at.unix_timestamp();
        let bucket_end = bucket.ended_at.unix_timestamp();
        let start = started_at.max(bucket_start);
        let end = ended_at.min(bucket_end);
        if end <= start {
            continue;
        }
        let secs = u64::try_from(end - start).unwrap_or(0);
        bucket.active_secs = bucket.active_secs.saturating_add(secs);
        if work_eligible {
            bucket.work_active_secs = bucket.work_active_secs.saturating_add(secs);
        }
    }
}

fn floor_time_bucket(
    at: OffsetDateTime,
    kind: TimeBucketKind,
    offset: UtcOffset,
) -> OffsetDateTime {
    let local = at.to_offset(offset);
    match kind {
        TimeBucketKind::Hour => local
            .date()
            .with_hms(local.hour(), 0, 0)
            .unwrap_or_else(|_| local.date().midnight())
            .assume_offset(offset),
        TimeBucketKind::Day => local_day_start(local.date(), offset),
        TimeBucketKind::Week => local_day_start(week_start_monday(local.date()), offset),
        TimeBucketKind::Month => month_start(local.date()).midnight().assume_offset(offset),
    }
}

fn advance_time_bucket(
    at: OffsetDateTime,
    kind: TimeBucketKind,
    offset: UtcOffset,
) -> OffsetDateTime {
    match kind {
        TimeBucketKind::Hour => at + time::Duration::hours(1),
        TimeBucketKind::Day => at + time::Duration::days(1),
        TimeBucketKind::Week => at + time::Duration::weeks(1),
        TimeBucketKind::Month => next_month_start(at, offset),
    }
}

fn time_bucket_label(
    at: OffsetDateTime,
    kind: TimeBucketKind,
    offset: UtcOffset,
    hour_includes_date: bool,
) -> String {
    let local = at.to_offset(offset);
    let formatted = match kind {
        TimeBucketKind::Hour if hour_includes_date => {
            local.format(time::macros::format_description!("[month]-[day] [hour]:00"))
        }
        TimeBucketKind::Hour => local.format(time::macros::format_description!("[hour]:00")),
        TimeBucketKind::Day | TimeBucketKind::Week => {
            local.format(time::macros::format_description!("[year]-[month]-[day]"))
        }
        TimeBucketKind::Month => local.format(time::macros::format_description!("[year]-[month]")),
    };
    formatted.unwrap_or_else(|_| local.to_string())
}

fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

fn local_day_start(date: Date, offset: UtcOffset) -> OffsetDateTime {
    date.midnight().assume_offset(offset)
}

fn week_start_monday(mut date: Date) -> Date {
    for _ in 0..weekday_days_from_monday(date.weekday()) {
        let Some(previous) = date.previous_day() else {
            break;
        };
        date = previous;
    }
    date
}

fn weekday_days_from_monday(weekday: Weekday) -> u8 {
    match weekday {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

fn month_start(date: Date) -> Date {
    Date::from_calendar_date(date.year(), date.month(), 1).unwrap_or(date)
}

fn next_month_start(at: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
    let local = at.to_offset(offset);
    let date = local.date();
    let month = u8::from(date.month());
    let (year, month) = if month == 12 {
        (date.year().saturating_add(1), Month::January)
    } else {
        (
            date.year(),
            Month::try_from(month.saturating_add(1)).unwrap_or(Month::December),
        )
    };
    Date::from_calendar_date(year, month, 1)
        .unwrap_or(date)
        .midnight()
        .assume_offset(offset)
}

fn add_state_transition_row(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
    entry: &StateTransitionEntry,
    secs: u64,
) {
    if secs == 0 && !data.range.includes(entry.at) {
        return;
    }
    let acc = rows
        .entry(state_transition_group_key(data, entry, group_by))
        .or_default();
    add_state_secs(
        entry.from,
        secs,
        &mut acc.working_secs,
        &mut acc.waiting_secs,
        &mut acc.error_secs,
    );
    if data.range.includes(entry.at) && is_attention_state(entry.to) {
        acc.attention_events += 1;
    }
}

fn prompt_group_key(data: &StatsData, prompt: &HistoryEntry, group_by: GroupBy) -> String {
    match group_by {
        GroupBy::Day => format_day(prompt.at),
        GroupBy::Project => prompt
            .cwd
            .as_deref()
            .and_then(|cwd| project_from_cwd(Some(cwd)))
            .or_else(|| {
                data.project_by_agent_session
                    .get(&prompt.session_id)
                    .cloned()
            })
            .or_else(|| data.project_by_pane.get(&prompt.pane).cloned())
            .unwrap_or_else(|| "unknown".to_string()),
        GroupBy::Agent => prompt.kind.to_string(),
        GroupBy::Session => prompt
            .tmux_session
            .clone()
            .or_else(|| data.pane_sessions.get(&prompt.pane).cloned())
            .unwrap_or_else(|| prompt.session_id.clone()),
    }
}

fn agent_group_key(data: &StatsData, agent: &Agent, group_by: GroupBy) -> String {
    match group_by {
        GroupBy::Day => format_day(agent.last_activity_at),
        GroupBy::Project => {
            project_from_cwd(agent.cwd.as_deref()).unwrap_or_else(|| "unknown".to_string())
        }
        GroupBy::Agent => agent.kind.to_string(),
        GroupBy::Session => agent
            .pane
            .as_ref()
            .and_then(|pane| data.pane_sessions.get(pane))
            .cloned()
            .unwrap_or_else(|| agent.session_id.clone()),
    }
}

fn state_transition_group_key(
    data: &StatsData,
    entry: &StateTransitionEntry,
    group_by: GroupBy,
) -> String {
    match group_by {
        GroupBy::Day => format_day(entry.at),
        GroupBy::Project => entry
            .cwd
            .as_deref()
            .and_then(|cwd| project_from_cwd(Some(cwd)))
            .or_else(|| {
                data.project_by_agent_session
                    .get(&entry.session_id)
                    .cloned()
            })
            .or_else(|| {
                entry
                    .pane
                    .as_ref()
                    .and_then(|pane| data.project_by_pane.get(pane))
                    .cloned()
            })
            .unwrap_or_else(|| "unknown".to_string()),
        GroupBy::Agent => entry.kind.to_string(),
        GroupBy::Session => entry
            .session_name
            .clone()
            .or_else(|| {
                entry
                    .pane
                    .as_ref()
                    .and_then(|pane| data.pane_sessions.get(pane))
                    .cloned()
            })
            .unwrap_or_else(|| entry.session_id.clone()),
    }
}

fn session_foreground_group_key(
    entry: &SessionForegroundEntry,
    group_by: GroupBy,
) -> Option<String> {
    match group_by {
        GroupBy::Day => Some(format_day(entry.ended_at)),
        GroupBy::Session => Some(entry.session_name.clone()),
        GroupBy::Project | GroupBy::Agent => None,
    }
}
fn add_open_session_foreground_rows(
    data: &StatsData,
    group_by: GroupBy,
    rows: &mut BTreeMap<String, GroupAccumulator>,
) {
    if !matches!(group_by, GroupBy::Day | GroupBy::Session) {
        return;
    }
    for activity in &data.activities {
        let Some(since) = activity.attached_since else {
            continue;
        };
        let secs = overlap_secs(data, since, data.now);
        if secs == 0 {
            continue;
        }
        let key = match group_by {
            GroupBy::Day => format_day(data.now),
            GroupBy::Session => activity.name.clone(),
            GroupBy::Project | GroupBy::Agent => unreachable!(),
        };
        rows.entry(key).or_default().foreground_secs += secs;
    }
}

fn state_transition_overlap_secs(data: &StatsData, entry: &StateTransitionEntry) -> u64 {
    let started_at = entry.state_entered_at.unwrap_or_else(|| {
        entry.at - time::Duration::seconds(i64::try_from(entry.duration_secs).unwrap_or(i64::MAX))
    });
    overlap_secs(data, started_at, entry.at)
}

fn session_foreground_overlap_secs(data: &StatsData, entry: &SessionForegroundEntry) -> u64 {
    overlap_secs(data, entry.started_at, entry.ended_at)
}

fn open_session_foreground_secs(data: &StatsData) -> u64 {
    data.activities
        .iter()
        .filter_map(|activity| activity.attached_since)
        .map(|since| overlap_secs(data, since, data.now))
        .sum()
}

fn open_agent_state_secs(data: &StatsData, agent: &Agent) -> u64 {
    if agent.state == AgentState::Stopped {
        return 0;
    }
    overlap_secs(data, agent.state_entered_at, data.now)
}

fn legacy_foreground_secs(data: &StatsData) -> u64 {
    data.activities
        .iter()
        .filter(|activity| activity.attached_since.is_none())
        .map(|activity| activity.effective_total_secs(data.now))
        .sum()
}

fn has_session_foreground_ledger(data: &StatsData) -> bool {
    data.activity_entries
        .iter()
        .any(|entry| matches!(entry, ActivityEntry::SessionForeground(_)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HumanPresenceMode {
    /// Raw HUMAN: foreground plus muxa-recorded human intervals.
    Human,
    /// Presence strong enough to bound ACT/WACT padding.
    Active,
    /// Presence strong enough to count time spent resolving attention states.
    Thinking,
}

fn human_presence_intervals(data: &StatsData, mode: HumanPresenceMode) -> Vec<ScopedInterval> {
    let mut intervals = Vec::new();
    for entry in &data.activity_entries {
        match entry {
            ActivityEntry::SessionForeground(entry) => {
                if let Some(interval) = scoped_interval(
                    data,
                    entry.started_at,
                    entry.ended_at,
                    None,
                    Some(entry.session_name.clone()),
                    &entry.session_id,
                ) {
                    intervals.push(interval);
                }
            }
            ActivityEntry::HumanInteraction(entry) => {
                // Input ticks (keypress/scroll) are instantaneous input *markers*,
                // not presence spans — they feed `active` directly, never HUMAN or
                // THINK, so a stream of ticks can't inflate raw presence.
                if entry.kind.is_input_tick() {
                    continue;
                }
                if mode == HumanPresenceMode::Active
                    && !human_interaction_counts_for_active(entry.kind)
                {
                    continue;
                }
                if mode == HumanPresenceMode::Thinking
                    && !human_interaction_counts_for_thinking(entry.kind)
                {
                    continue;
                }
                if let Some(interval) = scoped_interval(
                    data,
                    entry.started_at,
                    entry.ended_at,
                    entry.pane.clone(),
                    entry.session_name.clone(),
                    entry.session_id.as_deref().unwrap_or("human_interaction"),
                ) {
                    intervals.push(interval);
                }
            }
            ActivityEntry::StateTransition(_) => {}
        }
    }
    for activity in &data.activities {
        let Some(since) = activity.attached_since else {
            continue;
        };
        if let Some(interval) = scoped_interval(
            data,
            since,
            data.now,
            None,
            Some(activity.name.clone()),
            &activity.session_id,
        ) {
            intervals.push(interval);
        }
    }
    intervals
}

fn human_interaction_counts_for_active(kind: HumanInteractionKind) -> bool {
    matches!(
        kind,
        HumanInteractionKind::MuxaPromptInput | HumanInteractionKind::TmuxAttach
    )
}

fn human_interaction_counts_for_thinking(kind: HumanInteractionKind) -> bool {
    matches!(
        kind,
        HumanInteractionKind::MuxaPromptInput | HumanInteractionKind::TmuxAttach
    )
}

fn human_presence_group_key(
    data: &StatsData,
    interval: &ScopedInterval,
    group_by: GroupBy,
) -> Option<String> {
    match group_by {
        GroupBy::Day => Some(format_day(interval.ended_at)),
        GroupBy::Session => interval
            .session_name
            .clone()
            .or_else(|| interval.pane.clone())
            .or_else(|| Some("unknown".to_string())),
        GroupBy::Project => interval
            .pane
            .as_ref()
            .and_then(|pane| data.project_by_pane.get(pane))
            .cloned(),
        GroupBy::Agent => None,
    }
}

fn attention_intervals(data: &StatsData, group_by: GroupBy) -> Vec<AttentionInterval> {
    let mut intervals = Vec::new();
    for entry in &data.activity_entries {
        let ActivityEntry::StateTransition(entry) = entry else {
            continue;
        };
        if !is_attention_state(entry.from) {
            continue;
        }
        let started_at = entry.state_entered_at.unwrap_or_else(|| {
            entry.at
                - time::Duration::seconds(i64::try_from(entry.duration_secs).unwrap_or(i64::MAX))
        });
        let session_name = entry.session_name.clone().or_else(|| {
            entry
                .pane
                .as_ref()
                .and_then(|pane| data.pane_sessions.get(pane))
                .cloned()
        });
        if let Some(interval) = scoped_interval(
            data,
            started_at,
            entry.at,
            entry.pane.clone(),
            session_name,
            &entry.session_id,
        ) {
            let anchor = interval.started_at;
            intervals.push(AttentionInterval {
                interval,
                group_key: state_transition_group_key(data, entry, group_by),
                work_eligible: true,
                anchor,
            });
        }
    }
    for agent in &data.agents {
        if !is_attention_state(agent.state) {
            continue;
        }
        let session_name = agent
            .pane
            .as_ref()
            .and_then(|pane| data.pane_sessions.get(pane))
            .cloned();
        if let Some(interval) = scoped_interval(
            data,
            agent.state_entered_at,
            data.now,
            agent.pane.clone(),
            session_name,
            &agent.session_id,
        ) {
            let anchor = interval.started_at;
            intervals.push(AttentionInterval {
                interval,
                group_key: agent_group_key(data, agent, group_by),
                work_eligible: true,
                anchor,
            });
        }
    }
    intervals
}

fn thinking_secs_total(data: &StatsData) -> u64 {
    let presences = human_presence_intervals(data, HumanPresenceMode::Thinking);
    let mut segments = Vec::new();
    for attention in attention_intervals(data, GroupBy::Session) {
        segments.extend(overlapping_presence_segments(
            &attention.interval,
            &presences,
        ));
    }
    sum_merged_scoped_intervals(&segments)
}

fn overlapping_presence_segments(
    attention: &ScopedInterval,
    presences: &[ScopedInterval],
) -> Vec<ScopedInterval> {
    let mut segments = Vec::new();
    for presence in presences {
        if !intervals_relate(attention, presence) {
            continue;
        }
        let start = attention.started_at.max(presence.started_at);
        let end = attention.ended_at.min(presence.ended_at);
        if end <= start {
            continue;
        }
        segments.push(ScopedInterval {
            started_at: start,
            ended_at: end,
            pane: attention.pane.clone(),
            session_name: attention.session_name.clone(),
            scope_key: attention.scope_key.clone(),
        });
    }
    segments
}

fn intervals_relate(a: &ScopedInterval, b: &ScopedInterval) -> bool {
    if let (Some(a_pane), Some(b_pane)) = (a.pane.as_deref(), b.pane.as_deref()) {
        if a_pane == b_pane {
            return true;
        }
    }
    if let (Some(a_session), Some(b_session)) =
        (a.session_name.as_deref(), b.session_name.as_deref())
    {
        return a_session == b_session;
    }
    false
}

fn scoped_interval(
    data: &StatsData,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
    pane: Option<String>,
    session_name: Option<String>,
    fallback_scope: &str,
) -> Option<ScopedInterval> {
    let (start, end) = clip_interval(data, started_at, ended_at)?;
    let scope_key = scope_key(pane.as_deref(), session_name.as_deref(), fallback_scope);
    Some(ScopedInterval {
        started_at: start,
        ended_at: end,
        pane,
        session_name,
        scope_key,
    })
}

fn clip_interval(
    data: &StatsData,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let start = data
        .range
        .since_at
        .map_or(started_at, |since| started_at.max(since));
    let end = ended_at.min(data.range.effective_end(data.now));
    (end > start).then_some((start, end))
}

fn scope_key(pane: Option<&str>, session_name: Option<&str>, fallback: &str) -> String {
    if let Some(session_name) = session_name {
        return format!("session:{session_name}");
    }
    if let Some(pane) = pane {
        return format!("pane:{pane}");
    }
    format!("unknown:{fallback}")
}

fn sum_merged_scoped_intervals(intervals: &[ScopedInterval]) -> u64 {
    let mut by_scope: BTreeMap<&str, Vec<(OffsetDateTime, OffsetDateTime)>> = BTreeMap::new();
    for interval in intervals {
        if interval.ended_at <= interval.started_at {
            continue;
        }
        by_scope
            .entry(interval.scope_key.as_str())
            .or_default()
            .push((interval.started_at, interval.ended_at));
    }

    let mut total = 0u64;
    for mut ranges in by_scope.into_values() {
        ranges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut merged_start: Option<OffsetDateTime> = None;
        let mut merged_end: Option<OffsetDateTime> = None;
        for (start, end) in ranges {
            match (merged_start, merged_end) {
                (None, None) => {
                    merged_start = Some(start);
                    merged_end = Some(end);
                }
                (Some(current_start), Some(current_end)) if start <= current_end => {
                    merged_start = Some(current_start);
                    merged_end = Some(current_end.max(end));
                }
                (Some(current_start), Some(current_end)) => {
                    total = total.saturating_add(duration_between(current_start, current_end));
                    merged_start = Some(start);
                    merged_end = Some(end);
                }
                _ => unreachable!("merged interval state is always both set or both unset"),
            }
        }
        if let (Some(start), Some(end)) = (merged_start, merged_end) {
            total = total.saturating_add(duration_between(start, end));
        }
    }
    total
}

fn duration_between(started_at: OffsetDateTime, ended_at: OffsetDateTime) -> u64 {
    u64::try_from((ended_at - started_at).whole_seconds().max(0)).unwrap_or(u64::MAX)
}

fn overlap_secs(data: &StatsData, started_at: OffsetDateTime, ended_at: OffsetDateTime) -> u64 {
    let start = data
        .range
        .since_at
        .map_or(started_at, |since| started_at.max(since));
    let end = ended_at.min(data.range.effective_end(data.now));
    if end <= start {
        return 0;
    }
    u64::try_from((end - start).whole_seconds()).unwrap_or(u64::MAX)
}

fn add_state_secs(
    state: AgentState,
    secs: u64,
    working_secs: &mut u64,
    waiting_secs: &mut u64,
    error_secs: &mut u64,
) {
    match state {
        AgentState::Working => *working_secs = working_secs.saturating_add(secs),
        AgentState::WaitingInput | AgentState::WaitingChoice => {
            *waiting_secs = waiting_secs.saturating_add(secs);
        }
        AgentState::Error => *error_secs = error_secs.saturating_add(secs),
        AgentState::Starting | AgentState::Idle | AgentState::Stopped => {}
    }
}

fn is_attention_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

#[derive(Debug, Clone, Copy)]
struct PromptMetrics {
    chars: usize,
    words: usize,
    token_estimate: usize,
}

fn prompt_metrics(prompt: &HistoryEntry) -> PromptMetrics {
    let chars = prompt.prompt.chars().count();
    PromptMetrics {
        chars,
        words: prompt.prompt.split_whitespace().count(),
        token_estimate: chars / 4,
    }
}

fn parse_since(raw: &str, now: OffsetDateTime) -> Result<TimeRange> {
    crate::time_range::parse_since(raw, now, "all retained history")
}

fn project_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim();
    if cwd.is_empty() {
        return None;
    }
    let name = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(cwd);
    Some(name.to_string())
}

fn update_max_time(current: &mut Option<OffsetDateTime>, candidate: OffsetDateTime) {
    *current = Some(current.map_or(candidate, |existing| existing.max(candidate)));
}

fn notes(data: &StatsData) -> Vec<String> {
    let mut notes = vec![
        "Prompt totals cover the daemon's retained history window, bounded by [history].max_per_pane and max_age_days.".to_string(),
    ];
    if data.activity_entries.is_empty() {
        notes.push(
            "No activity ledger entries found yet; duration columns will fill as agents transition, tmux foreground intervals close, and muxa interactions are recorded.".to_string(),
        );
    } else if !has_session_foreground_ledger(data) && !data.activities.is_empty() {
        notes.push(
            "TMUX uses legacy cumulative session-activity.json totals until the first session foreground interval lands in activity.ndjson.".to_string(),
        );
    }
    notes.push(
        "THINK is the overlap of attention states (WaitingInput, WaitingChoice, Error) with human presence (tmux foreground, prompt input, or tmux attach).".to_string(),
    );
    notes.push(format!(
        "ACTIVE estimates engaged human time: the union of (a) windows around each submitted prompt, padded {lookback}s before / {prompt_timeout}s after, (b) tmux input ticks recorded when a client's activity advances (keypress/scroll, so reading counts too), padded {lookback}s before / {tick_timeout}s after, and (c) thinking (present while an agent is blocked on you). Prompt/input windows are clipped to matching active presence (tmux foreground, prompt input, or tmux attach; plain muxa_watch does not extend ACT), so padding cannot outlive observed foreground/interaction time. Overlapping windows across concurrent sessions are de-duplicated (last touch), so per-row ACTIVE sums to the total. An idle attach advances none of these, so ACTIVE discounts it.",
        lookback = data.active_lookback.whole_seconds().max(0),
        prompt_timeout = data.active_timeout.whole_seconds().max(0),
        tick_timeout = data.active_tick_timeout.whole_seconds().max(0),
    ));
    notes.push(
        "WACT is the hands-on subset of ACT: the same last-touch owner is used, but seconds owned by scrollback-only windows are excluded, so each row's WACT stays within its ACT.".to_string(),
    );
    notes.push(
        "WORK/WAIT/ERR sum every tracked agent independently (WORK = agent busy, WAIT = agent blocked on you, ERR = agent errored), so with agents running in parallel a single day's WORK can exceed 24h of wall-clock. BLK counts attention events.".to_string(),
    );
    notes
}

fn render_table(doc: &StatsDocument, theme: CliTheme, verbose: bool) {
    print_range_header("muxa stats", doc);
    println!();

    let terminal_width = terminal_width();
    if let Some(graph) = &doc.time_graph {
        println!("{}", render_time_graph(graph, terminal_width, theme));
        return;
    }

    if doc.rows.is_empty() {
        println!("no retained prompts, live agents, or tracked session activity in this view");
    } else {
        println!(
            "{}",
            render_stats_table(doc, terminal_width, theme, verbose)
        );
        if verbose && stats_table_layout(terminal_width) != StatsTableLayout::Full {
            println!("{}", compaction_hint());
        }
    }

    print_notes(&doc.notes, verbose, "muxa stats --verbose");
}

/// Table output for `muxa report`: the same range header and per-section tables
/// as `stats`, one table per group-by dimension. Totals/notes are identical
/// across the documents (they aggregate the same data), so the range header and
/// notes are printed once, from the first document.
fn render_report_tables(docs: &[StatsDocument], theme: CliTheme, verbose: bool) {
    let Some(first) = docs.first() else {
        return;
    };
    print_range_header("muxa report", first);
    println!();

    let terminal_width = terminal_width();
    let mut compacted = false;
    for doc in docs {
        println!("By {}", GroupByLabel(&doc.group_by));
        if doc.rows.is_empty() {
            println!("no rows in this view");
        } else {
            println!(
                "{}",
                render_stats_table(doc, terminal_width, theme, verbose)
            );
            compacted |= verbose && stats_table_layout(terminal_width) != StatsTableLayout::Full;
        }
        println!();
    }
    if compacted {
        println!("{}", compaction_hint());
    }

    print_notes(&first.notes, verbose, "muxa report --verbose");
}

fn print_range_header(title: &str, doc: &StatsDocument) {
    println!("{title}");
    println!("Range: {}", doc.range.label);
    if let Some(since_at) = doc.range.since_instant {
        println!("Since: {}", format_local_seconds(since_at));
    }
    if let Some(until_at) = doc.range.until_instant {
        println!("Until: {}", format_local_seconds(until_at));
    }
}

fn compaction_hint() -> &'static str {
    "note: Diagnostic table compacted for terminal width; use --json or --markdown for every field."
}

fn print_notes(notes: &[String], verbose: bool, verbose_cmd: &str) {
    if notes.is_empty() {
        return;
    }
    if verbose {
        for note in notes {
            println!("note: {note}");
        }
    } else {
        // A compact legend so the default view is legible without reading the
        // docs. WORK/WAIT sum concurrent agents, so a day can exceed 24h — call
        // that out here since it otherwise reads as a data error.
        println!(
            "legend: WACT=hands-on you · ACT=engaged you · WORK=agent busy (Σ concurrent, may exceed 24h) · WAIT=agent blocked on you · BLK=attention events · PRM=prompts",
        );
        println!(
            "note: {n} explanatory note{plural} hidden; run `{verbose_cmd}` for methodology, or `muxa doctor` to check health.",
            n = notes.len(),
            plural = if notes.len() == 1 { "" } else { "s" },
        );
    }
}

fn render_time_graph(graph: &TimeGraph, terminal_width: usize, theme: CliTheme) -> String {
    if graph.max_secs == 0 {
        return format!(
            "WACT graph · {} · no work-active time in this range",
            graph.bucket
        );
    }

    let raw_label_width = graph
        .buckets
        .iter()
        .map(|bucket| bucket.label.len())
        .max()
        .unwrap_or(6)
        .max("BUCKET".len())
        .clamp(6, 12);
    let value_width = graph
        .buckets
        .iter()
        .map(|bucket| bucket.work_active.len())
        .chain(std::iter::once(graph.total.len()))
        .max()
        .unwrap_or(1)
        .clamp(1, 8);
    let max_label_width = terminal_width
        .saturating_sub(value_width.saturating_mul(2) + 14)
        .clamp(1, 12);
    let label_width = raw_label_width.min(max_label_width);
    let reserved_width = label_width + value_width.saturating_mul(2) + 13;
    let bar_width = terminal_width.saturating_sub(reserved_width).clamp(1, 56);

    let mut out = String::new();
    let header = format!(
        "WACT over time · {} buckets · total {} · peak {}",
        graph.bucket, graph.total, graph.max
    );
    let _ = writeln!(out, "{}", truncate_cell(&header, terminal_width.max(1)));

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints([
            ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(label_width).unwrap_or(u16::MAX),
            )),
            ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(value_width).unwrap_or(u16::MAX),
            )),
            ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(value_width).unwrap_or(u16::MAX),
            )),
            ColumnConstraint::Absolute(Width::Fixed(u16::try_from(bar_width).unwrap_or(u16::MAX))),
        ])
        .set_header([
            theme.cell(truncate_cell("BUCKET", label_width), TableTone::Header),
            theme.right_cell(truncate_cell("WACT", value_width), TableTone::Header),
            theme.right_cell(truncate_cell("ACT", value_width), TableTone::Header),
            theme.cell(truncate_cell("DISTRIBUTION", bar_width), TableTone::Header),
        ]);

    for bucket in &graph.buckets {
        table.add_row([
            theme.cell(truncate_cell(&bucket.label, label_width), TableTone::Accent),
            theme.right_cell(
                truncate_cell(&bucket.work_active, value_width),
                TableTone::Good,
            ),
            theme.right_cell(truncate_cell(&bucket.active, value_width), TableTone::Human),
            theme.cell(
                smooth_bar(bucket.work_active_secs, graph.max_secs, bar_width),
                TableTone::Good,
            ),
        ]);
    }

    let _ = write!(out, "{table}");
    out.trim_end().to_string()
}

fn smooth_bar(secs: u64, max_secs: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if secs == 0 || max_secs == 0 {
        return " ".repeat(width);
    }
    let width_units = u128::try_from(width).unwrap_or(u128::MAX).saturating_mul(8);
    let units = u128::from(secs)
        .saturating_mul(width_units)
        .div_ceil(u128::from(max_secs))
        .clamp(1, width_units);
    let full = usize::try_from(units / 8).unwrap_or(width).min(width);
    let partial = usize::try_from(units % 8).unwrap_or(0);

    let mut out = String::new();
    out.push_str(&"█".repeat(full));
    if full < width && partial > 0 {
        out.push(partial_block(partial));
    }
    let used = full + usize::from(full < width && partial > 0);
    out.push_str(&"░".repeat(width.saturating_sub(used)));
    out
}

fn partial_block(eighths: usize) -> char {
    match eighths {
        1 => '▏',
        2 => '▎',
        3 => '▍',
        4 => '▌',
        5 => '▋',
        6 => '▊',
        7 => '▉',
        _ => '█',
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsTableLayout {
    Full,
    Compact,
    Minimal,
}

#[derive(Debug, Clone, Copy)]
enum StatsColumn {
    Prompts,
    Working,
    Waiting,
    Error,
    Foreground,
    Human,
    Thinking,
    Active,
    WorkActive,
    AttentionEvents,
    TokenEstimate,
    AgentSessions,
    LiveAgents,
    LastPromptAge,
}

#[derive(Debug, Clone, Copy)]
struct StatsTableColumn {
    header: &'static str,
    width: usize,
    value: StatsColumn,
}

const FULL_STATS_COLUMNS: &[StatsTableColumn] = &[
    StatsTableColumn {
        header: "PROMPTS",
        width: 7,
        value: StatsColumn::Prompts,
    },
    StatsTableColumn {
        header: "WORK",
        width: 6,
        value: StatsColumn::Working,
    },
    StatsTableColumn {
        header: "WAIT",
        width: 6,
        value: StatsColumn::Waiting,
    },
    StatsTableColumn {
        header: "ERR",
        width: 5,
        value: StatsColumn::Error,
    },
    StatsTableColumn {
        header: "TMUX",
        width: 6,
        value: StatsColumn::Foreground,
    },
    StatsTableColumn {
        header: "HUMAN",
        width: 6,
        value: StatsColumn::Human,
    },
    StatsTableColumn {
        header: "ACT",
        width: 6,
        value: StatsColumn::Active,
    },
    StatsTableColumn {
        header: "WACT",
        width: 6,
        value: StatsColumn::WorkActive,
    },
    StatsTableColumn {
        header: "THINK",
        width: 6,
        value: StatsColumn::Thinking,
    },
    StatsTableColumn {
        header: "BLOCK",
        width: 5,
        value: StatsColumn::AttentionEvents,
    },
    StatsTableColumn {
        header: "TOK EST",
        width: 7,
        value: StatsColumn::TokenEstimate,
    },
    StatsTableColumn {
        header: "SESS",
        width: 4,
        value: StatsColumn::AgentSessions,
    },
    StatsTableColumn {
        header: "AGENTS",
        width: 6,
        value: StatsColumn::LiveAgents,
    },
    StatsTableColumn {
        header: "LAST",
        width: 7,
        value: StatsColumn::LastPromptAge,
    },
];

const SUMMARY_FULL_STATS_COLUMNS: &[StatsTableColumn] = &[
    StatsTableColumn {
        header: "WACT",
        width: 6,
        value: StatsColumn::WorkActive,
    },
    StatsTableColumn {
        header: "ACT",
        width: 6,
        value: StatsColumn::Active,
    },
    StatsTableColumn {
        header: "WORK",
        width: 6,
        value: StatsColumn::Working,
    },
    StatsTableColumn {
        header: "WAIT",
        width: 6,
        value: StatsColumn::Waiting,
    },
    StatsTableColumn {
        header: "BLK",
        width: 5,
        value: StatsColumn::AttentionEvents,
    },
    StatsTableColumn {
        header: "PROMPTS",
        width: 7,
        value: StatsColumn::Prompts,
    },
    StatsTableColumn {
        header: "LAST",
        width: 7,
        value: StatsColumn::LastPromptAge,
    },
];

const SUMMARY_COMPACT_STATS_COLUMNS: &[StatsTableColumn] = &[
    StatsTableColumn {
        header: "WACT",
        width: 6,
        value: StatsColumn::WorkActive,
    },
    StatsTableColumn {
        header: "ACT",
        width: 6,
        value: StatsColumn::Active,
    },
    StatsTableColumn {
        header: "WORK",
        width: 6,
        value: StatsColumn::Working,
    },
    StatsTableColumn {
        header: "WAIT",
        width: 6,
        value: StatsColumn::Waiting,
    },
    StatsTableColumn {
        header: "BLK",
        width: 5,
        value: StatsColumn::AttentionEvents,
    },
    StatsTableColumn {
        header: "PRM",
        width: 5,
        value: StatsColumn::Prompts,
    },
    StatsTableColumn {
        header: "LAST",
        width: 7,
        value: StatsColumn::LastPromptAge,
    },
];

const SUMMARY_MINIMAL_STATS_COLUMNS: &[StatsTableColumn] = &[
    StatsTableColumn {
        header: "WACT",
        width: 6,
        value: StatsColumn::WorkActive,
    },
    StatsTableColumn {
        header: "ACT",
        width: 6,
        value: StatsColumn::Active,
    },
    StatsTableColumn {
        header: "PRM",
        width: 5,
        value: StatsColumn::Prompts,
    },
    StatsTableColumn {
        header: "LAST",
        width: 7,
        value: StatsColumn::LastPromptAge,
    },
];

const COMPACT_STATS_COLUMNS: &[StatsTableColumn] = &[
    StatsTableColumn {
        header: "PRM",
        width: 5,
        value: StatsColumn::Prompts,
    },
    StatsTableColumn {
        header: "WORK",
        width: 6,
        value: StatsColumn::Working,
    },
    StatsTableColumn {
        header: "WAIT",
        width: 6,
        value: StatsColumn::Waiting,
    },
    StatsTableColumn {
        header: "TMUX",
        width: 6,
        value: StatsColumn::Foreground,
    },
    StatsTableColumn {
        header: "ACT",
        width: 6,
        value: StatsColumn::Active,
    },
    StatsTableColumn {
        header: "THINK",
        width: 6,
        value: StatsColumn::Thinking,
    },
    StatsTableColumn {
        header: "LAST",
        width: 7,
        value: StatsColumn::LastPromptAge,
    },
];

const MINIMAL_STATS_COLUMNS: &[StatsTableColumn] = &[
    StatsTableColumn {
        header: "PRM",
        width: 5,
        value: StatsColumn::Prompts,
    },
    StatsTableColumn {
        header: "WORK",
        width: 6,
        value: StatsColumn::Working,
    },
    StatsTableColumn {
        header: "WAIT",
        width: 6,
        value: StatsColumn::Waiting,
    },
    StatsTableColumn {
        header: "THINK",
        width: 6,
        value: StatsColumn::Thinking,
    },
    StatsTableColumn {
        header: "LAST",
        width: 7,
        value: StatsColumn::LastPromptAge,
    },
];

fn render_stats_table(
    doc: &StatsDocument,
    terminal_width: usize,
    theme: CliTheme,
    verbose: bool,
) -> String {
    let layout = stats_table_layout(terminal_width);
    let columns = stats_table_columns(layout, verbose);
    let group_width = stats_group_column_width(terminal_width, columns);
    let mut table = Table::new();
    let mut constraints = Vec::with_capacity(columns.len() + 1);
    constraints.push(ColumnConstraint::Absolute(Width::Fixed(
        u16::try_from(group_width).unwrap_or(u16::MAX),
    )));
    constraints.extend(columns.iter().map(|column| {
        ColumnConstraint::Absolute(Width::Fixed(
            u16::try_from(column.width).unwrap_or(u16::MAX),
        ))
    }));

    let mut header = Vec::with_capacity(columns.len() + 1);
    header.push(theme.cell(
        truncate_cell(&doc.group_by.to_ascii_uppercase(), group_width),
        TableTone::Header,
    ));
    header.extend(columns.iter().map(|column| {
        theme.right_cell(
            truncate_cell(column.header, column.width),
            TableTone::Header,
        )
    }));

    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints(constraints)
        .set_header(header);

    for row in &doc.rows {
        let mut cells = Vec::with_capacity(columns.len() + 1);
        cells.push(theme.cell(truncate_cell(&row.key, group_width), TableTone::Accent));
        cells.extend(columns.iter().map(|column| {
            theme.right_cell(
                truncate_cell(&stats_column_value(row, column.value), column.width),
                stats_column_tone(column.value),
            )
        }));
        table.add_row(cells);
    }

    // Grand-total footer. Built from `doc.totals`, so it reflects every group
    // even when `--limit` truncates the visible rows above.
    let mut total_cells = Vec::with_capacity(columns.len() + 1);
    total_cells.push(theme.cell(truncate_cell("TOTAL", group_width), TableTone::Header));
    total_cells.extend(columns.iter().map(|column| {
        theme.right_cell(
            truncate_cell(&stats_total_value(&doc.totals, column.value), column.width),
            stats_column_tone(column.value),
        )
    }));
    table.add_row(total_cells);

    insert_total_separator(&format!("{table}"))
}

/// Mirror the header rule above the TOTAL footer so the grand total reads as a
/// footer rather than another group row. Depends on the `UTF8_BORDERS_ONLY`
/// preset, whose only `╞`-led line is the header separator.
fn insert_total_separator(rendered: &str) -> String {
    let lines: Vec<&str> = rendered.lines().collect();
    let Some(separator) = lines.iter().copied().find(|line| line.starts_with('╞')) else {
        return rendered.to_string();
    };
    if lines.len() < 4 {
        return rendered.to_string();
    }
    // The TOTAL row is added last, so it sits just above the bottom border.
    let total_idx = lines.len() - 2;
    let mut out = Vec::with_capacity(lines.len() + 1);
    for (idx, line) in lines.into_iter().enumerate() {
        if idx == total_idx {
            out.push(separator);
        }
        out.push(line);
    }
    out.join("\n")
}

fn stats_total_value(totals: &Totals, column: StatsColumn) -> String {
    match column {
        StatsColumn::Prompts => totals.prompts.to_string(),
        StatsColumn::Working => totals.working.clone(),
        StatsColumn::Waiting => totals.waiting.clone(),
        StatsColumn::Error => totals.error.clone(),
        StatsColumn::Foreground => totals.foreground.clone(),
        StatsColumn::Human => totals.human.clone(),
        StatsColumn::Thinking => totals.thinking.clone(),
        StatsColumn::Active => totals.active.clone(),
        StatsColumn::WorkActive => totals.work_active.clone(),
        StatsColumn::AttentionEvents => totals.attention_events.to_string(),
        StatsColumn::TokenEstimate => totals.token_estimate.to_string(),
        StatsColumn::AgentSessions => totals.agent_sessions.to_string(),
        StatsColumn::LiveAgents => totals.live_agents.to_string(),
        StatsColumn::LastPromptAge => totals.last_prompt_age.clone(),
    }
}

fn stats_column_tone(column: StatsColumn) -> TableTone {
    match column {
        StatsColumn::Prompts | StatsColumn::TokenEstimate | StatsColumn::AgentSessions => {
            TableTone::Accent
        }
        StatsColumn::Working
        | StatsColumn::LiveAgents
        | StatsColumn::Active
        | StatsColumn::WorkActive => TableTone::Good,
        StatsColumn::Waiting => TableTone::Warn,
        StatsColumn::Error => TableTone::Error,
        StatsColumn::Foreground => TableTone::Tmux,
        StatsColumn::Human => TableTone::Human,
        StatsColumn::Thinking => TableTone::Thinking,
        StatsColumn::AttentionEvents => TableTone::Choice,
        StatsColumn::LastPromptAge => TableTone::Dim,
    }
}

fn stats_table_layout(terminal_width: usize) -> StatsTableLayout {
    if terminal_width >= FULL_STATS_TABLE_WIDTH {
        StatsTableLayout::Full
    } else if terminal_width >= COMPACT_STATS_TABLE_WIDTH {
        StatsTableLayout::Compact
    } else {
        StatsTableLayout::Minimal
    }
}

fn stats_table_columns(layout: StatsTableLayout, verbose: bool) -> &'static [StatsTableColumn] {
    match (verbose, layout) {
        (true, StatsTableLayout::Full) => FULL_STATS_COLUMNS,
        (true, StatsTableLayout::Compact) => COMPACT_STATS_COLUMNS,
        (true, StatsTableLayout::Minimal) => MINIMAL_STATS_COLUMNS,
        (false, StatsTableLayout::Full) => SUMMARY_FULL_STATS_COLUMNS,
        (false, StatsTableLayout::Compact) => SUMMARY_COMPACT_STATS_COLUMNS,
        (false, StatsTableLayout::Minimal) => SUMMARY_MINIMAL_STATS_COLUMNS,
    }
}

fn stats_group_column_width(terminal_width: usize, columns: &[StatsTableColumn]) -> usize {
    let total_columns = columns.len() + 1;
    let fixed_content_width = columns.iter().map(|column| column.width).sum::<usize>();
    let border_and_padding_width = total_columns + 1 + total_columns * 2;
    terminal_width
        .saturating_sub(fixed_content_width + border_and_padding_width)
        .clamp(MIN_GROUP_COLUMN_WIDTH, MAX_GROUP_COLUMN_WIDTH)
}

fn stats_column_value(row: &GroupRow, column: StatsColumn) -> String {
    match column {
        StatsColumn::Prompts => row.prompts.to_string(),
        StatsColumn::Working => row.working.clone(),
        StatsColumn::Waiting => row.waiting.clone(),
        StatsColumn::Error => row.error.clone(),
        StatsColumn::Foreground => row.foreground.clone(),
        StatsColumn::Human => row.human.clone(),
        StatsColumn::Thinking => row.thinking.clone(),
        StatsColumn::Active => row.active.clone(),
        StatsColumn::WorkActive => row.work_active.clone(),
        StatsColumn::AttentionEvents => row.attention_events.to_string(),
        StatsColumn::TokenEstimate => row.token_estimate.to_string(),
        StatsColumn::AgentSessions => row.agent_sessions.to_string(),
        StatsColumn::LiveAgents => row.live_agents.to_string(),
        StatsColumn::LastPromptAge => row.last_prompt_age.clone(),
    }
}

fn render_markdown_stats(doc: &StatsDocument) -> String {
    let mut out = String::new();
    push_markdown_overview(&mut out, "muxa stats", doc);
    push_markdown_time_graph(&mut out, doc.time_graph.as_ref());
    push_markdown_rows(&mut out, &format!("By {}", doc.group_by), &doc.rows);
    push_markdown_notes(&mut out, &doc.notes);
    out
}

fn render_markdown_report(docs: &[StatsDocument]) -> String {
    let mut out = String::new();
    let Some(first) = docs.first() else {
        return out;
    };
    push_markdown_overview(&mut out, "muxa report", first);
    for doc in docs {
        push_markdown_rows(
            &mut out,
            &format!("By {}", GroupByLabel(&doc.group_by)),
            &doc.rows,
        );
    }
    push_markdown_notes(&mut out, &first.notes);
    out
}

struct GroupByLabel<'a>(&'a str);

impl std::fmt::Display for GroupByLabel<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self.0 {
            "day" => GroupBy::Day.heading(),
            "project" => GroupBy::Project.heading(),
            "agent" => GroupBy::Agent.heading(),
            "session" => GroupBy::Session.heading(),
            other => other,
        };
        f.write_str(label)
    }
}

fn push_markdown_overview(out: &mut String, title: &str, doc: &StatsDocument) {
    out.push_str("# ");
    out.push_str(title);
    out.push_str("\n\n");
    out.push_str("| Metric | Value |\n| --- | --- |\n");
    push_metric(
        out,
        "Generated",
        &format_local_seconds(doc.generated_instant),
    );
    push_metric(out, "Range", &doc.range.label);
    if let Some(since_at) = doc.range.since_instant {
        push_metric(out, "Since", &format_local_seconds(since_at));
    }
    if let Some(until_at) = doc.range.until_instant {
        push_metric(out, "Until", &format_local_seconds(until_at));
    }
    push_metric(out, "Prompts", &doc.totals.prompts.to_string());
    push_metric(
        out,
        "Token estimate",
        &doc.totals.token_estimate.to_string(),
    );
    push_metric(out, "Words", &doc.totals.words.to_string());
    push_metric(
        out,
        "Agent sessions",
        &doc.totals.agent_sessions.to_string(),
    );
    push_metric(out, "Live agents", &doc.totals.live_agents.to_string());
    push_metric(out, "Working", &doc.totals.working);
    push_metric(out, "Waiting", &doc.totals.waiting);
    push_metric(out, "Error time", &doc.totals.error);
    push_metric(out, "TMUX foreground", &doc.totals.foreground);
    push_metric(out, "Human presence", &doc.totals.human);
    push_metric(out, "Active (engaged)", &doc.totals.active);
    push_metric(out, "Work active (hands-on)", &doc.totals.work_active);
    push_metric(out, "Thinking", &doc.totals.thinking);
    push_metric(
        out,
        "Attention events",
        &doc.totals.attention_events.to_string(),
    );
    push_metric(out, "Last prompt", &doc.totals.last_prompt_age);
    out.push('\n');
}

fn push_metric(out: &mut String, key: &str, value: &str) {
    out.push_str("| ");
    out.push_str(&escape_markdown_cell(key));
    out.push_str(" | ");
    out.push_str(&escape_markdown_cell(value));
    out.push_str(" |\n");
}

fn push_markdown_rows(out: &mut String, title: &str, rows: &[GroupRow]) {
    out.push_str("## ");
    out.push_str(title);
    out.push_str("\n\n");
    if rows.is_empty() {
        out.push_str("_No rows._\n\n");
        return;
    }

    out.push_str("| Group | Prompts | Work | Wait | Error | TMUX | Human | Active | Think | Block | Tok est | Words | Sessions | Agents | Last |\n");
    out.push_str(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n",
    );
    for row in rows {
        out.push_str("| ");
        out.push_str(&escape_markdown_cell(&row.key));
        out.push_str(" | ");
        out.push_str(&row.prompts.to_string());
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.working));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.waiting));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.error));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.foreground));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.human));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.active));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.thinking));
        out.push_str(" | ");
        out.push_str(&row.attention_events.to_string());
        out.push_str(" | ");
        out.push_str(&row.token_estimate.to_string());
        out.push_str(" | ");
        out.push_str(&row.words.to_string());
        out.push_str(" | ");
        out.push_str(&row.agent_sessions.to_string());
        out.push_str(" | ");
        out.push_str(&row.live_agents.to_string());
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.last_prompt_age));
        out.push_str(" |\n");
    }
    out.push('\n');
}

fn push_markdown_time_graph(out: &mut String, graph: Option<&TimeGraph>) {
    let Some(graph) = graph else {
        return;
    };
    out.push_str("## Time Graph\n\n");
    out.push_str("| Bucket | WACT | ACT |\n");
    out.push_str("| --- | ---: | ---: |\n");
    for bucket in &graph.buckets {
        out.push_str("| ");
        out.push_str(&escape_markdown_cell(&bucket.label));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&bucket.work_active));
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&bucket.active));
        out.push_str(" |\n");
    }
    out.push('\n');
}

fn push_markdown_notes(out: &mut String, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    out.push_str("## Notes\n\n");
    for note in notes {
        out.push_str("- ");
        out.push_str(note);
        out.push('\n');
    }
}

fn escape_markdown_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn format_day(at: OffsetDateTime) -> String {
    // Bucket by the viewer's local calendar day, matching the range window and
    // the WACT graph (which both work in local time). Formatting the raw UTC
    // timestamp mislabels every day row by up to a day for non-UTC users and
    // splits "today" across two rows.
    let local = at.to_offset(local_offset());
    local
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_else(|_| local.date().to_string())
}

fn format_rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

/// Render a report-header instant in the viewer's local offset, truncated to
/// whole seconds, with the offset spelled out. This keeps the `Generated` /
/// `Since` / `Until` rows readable against the local calendar-day rows (which
/// also work in local time) instead of the raw UTC-microsecond RFC3339 string.
fn format_local_seconds(at: OffsetDateTime) -> String {
    let local = at.to_offset(local_offset());
    local
        .format(time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory]:[offset_minute]"
        ))
        .unwrap_or_else(|_| local.to_string())
}

fn relative_time(now: OffsetDateTime, then: OffsetDateTime) -> String {
    let secs = (now - then).whole_seconds().max(0);
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

fn format_duration(total_secs: u64) -> String {
    if total_secs == 0 {
        return "-".to_string();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::event::AgentKind;
    use muxa::{
        HumanInteractionEntry, HumanInteractionInput, HumanInteractionKind, StateTransitionInput,
    };
    use time::macros::datetime;
    use unicode_width::UnicodeWidthStr;

    fn prompt(
        kind: AgentKind,
        session_id: &str,
        pane: &str,
        cwd: Option<&str>,
        text: &str,
        at: OffsetDateTime,
    ) -> HistoryEntry {
        HistoryEntry::with_cwd(
            kind,
            session_id,
            pane,
            cwd.map(str::to_string),
            text,
            at,
            None,
        )
    }

    fn live_agent(state: AgentState, state_entered_at: OffsetDateTime, cwd: Option<&str>) -> Agent {
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::Codex,
            session_id: "agent-live".into(),
            surface: None,
            pane: Some("%1".into()),
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            cwd: cwd.map(str::to_string),
            state,
            last_prompt: None,
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: state_entered_at,
            last_activity_at: state_entered_at,
            state_entered_at,
        }
    }

    fn data(prompts: Vec<HistoryEntry>) -> StatsData {
        StatsData {
            now: datetime!(2026-05-30 12:00:00 UTC),
            range: TimeRange {
                label: "last 7d".into(),
                since_at: Some(datetime!(2026-05-23 12:00:00 UTC)),
                until_at: None,
            },
            prompts,
            activity_entries: Vec::new(),
            agents: Vec::new(),
            activities: Vec::new(),
            pane_sessions: HashMap::new(),
            project_by_pane: HashMap::new(),
            project_by_agent_session: HashMap::new(),
            active_lookback: time::Duration::seconds(60),
            active_timeout: time::Duration::seconds(300),
            // Match active_timeout in the shared fixture so existing prompt/tick
            // assertions are unaffected; tests exercising the shorter tick timeout
            // set this explicitly.
            active_tick_timeout: time::Duration::seconds(300),
            // Existing tick assertions assume ticks count; the opt-out is exercised
            // by its own test, which flips this to false.
            count_tmux_input: true,
        }
    }

    #[test]
    fn format_local_seconds_drops_microseconds_and_keeps_local_offset() {
        // A raw `Generated` instant carries sub-second precision and is UTC.
        let at = datetime!(2026-07-08 13:12:57.919314 UTC);
        let rendered = format_local_seconds(at);
        // The header must reconcile with the local-day rows: no microseconds and
        // no bare-UTC `Z`, rendered in the viewer's local offset.
        assert!(
            !rendered.contains('.'),
            "unexpected microseconds: {rendered}"
        );
        assert!(!rendered.contains('Z'), "unexpected Z suffix: {rendered}");
        let expected = at
            .to_offset(local_offset())
            .format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory]:[offset_minute]"
            ))
            .unwrap();
        assert_eq!(rendered, expected);
    }

    #[test]
    fn markdown_header_uses_local_seconds_not_rfc3339() {
        let mut d = data(Vec::new());
        d.now = datetime!(2026-07-08 13:12:57.919314 UTC);
        let doc = build_document(&d, GroupBy::Day, 10, SortKey::Name, false, false);
        let mut out = String::new();
        push_markdown_overview(&mut out, "Report", &doc);
        // JSON still carries the machine RFC3339 timestamp for consumers.
        assert!(doc.generated_at.contains('T') && doc.generated_at.ends_with('Z'));
        // The human header row does not leak the UTC-microsecond string.
        assert!(
            !out.contains("13:12:57.919314"),
            "markdown header leaked microseconds: {out}"
        );
        assert!(
            out.contains(&format_local_seconds(d.now)),
            "markdown header missing local-seconds Generated: {out}"
        );
    }

    #[test]
    fn parse_since_accepts_duration_units() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let range = parse_since("2h", now).unwrap();
        assert_eq!(range.label, "last 2h");
        assert_eq!(range.since_at, Some(datetime!(2026-05-30 10:00:00 UTC)));
    }

    #[test]
    fn parse_since_accepts_all() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let range = parse_since("all", now).unwrap();
        assert_eq!(range.label, "all retained history");
        assert_eq!(range.since_at, None);
        assert_eq!(range.until_at, None);
    }

    #[test]
    fn parse_since_accepts_week_alias() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let range = parse_since("week", now).unwrap();
        assert_eq!(range.label, "last 7d");
        assert_eq!(range.since_at, Some(datetime!(2026-05-23 12:00:00 UTC)));
        assert_eq!(range.until_at, None);
    }

    #[test]
    fn ended_range_clips_open_agent_state() {
        let mut d = data(Vec::new());
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 11:00:00 UTC)),
        };
        d.agents.push(live_agent(
            AgentState::WaitingInput,
            datetime!(2026-05-30 10:30:00 UTC),
            Some("/home/june/muxa"),
        ));

        let totals = build_totals(&d);

        assert_eq!(totals.waiting_secs, 1_800);
    }

    #[test]
    fn active_excludes_idle_attach() {
        let p1 = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "do a thing",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        let p2 = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "another",
            datetime!(2026-05-30 10:33:00 UTC),
        );
        let mut d = data(vec![p1, p2]);
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.pane_sessions.insert("%1".into(), "main".into());
        // A two-hour tmux attach with no other activity inflates HUMAN but,
        // having no anchoring action, must not inflate ACTIVE.
        d.activity_entries = vec![ActivityEntry::HumanInteraction(HumanInteractionEntry::new(
            HumanInteractionInput {
                kind: HumanInteractionKind::TmuxAttach,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 10:00:00 UTC),
                ended_at: datetime!(2026-05-30 12:00:00 UTC),
            },
        ))];

        let totals = build_totals(&d);

        // Raw presence counts the whole attach.
        assert_eq!(totals.human_secs, 7_200);
        // Active = union of [t-60s, t+300s] around the two prompts (10:29–10:38).
        assert_eq!(totals.active_secs, 540);
        assert!(totals.active_secs < totals.human_secs);
    }

    #[test]
    fn active_counts_tmux_input_reading() {
        // No prompts at all — only a long idle attach plus a single tmux input
        // tick (the human scrolled/typed at 10:30 while reading). Active must
        // credit the reading window, not the whole attach.
        let mut d = data(Vec::new());
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.pane_sessions.insert("%1".into(), "main".into());
        d.activity_entries = vec![
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxAttach,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 10:00:00 UTC),
                ended_at: datetime!(2026-05-30 12:00:00 UTC),
            })),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxInput,
                pane: None,
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 10:29:59 UTC),
                ended_at: datetime!(2026-05-30 10:30:00 UTC),
            })),
        ];

        let totals = build_totals(&d);

        // Window = [10:28:59, 10:35:00] around the 1s tick = 6m01s.
        assert_eq!(totals.active_secs, 361);
        assert!(totals.active_secs < totals.human_secs);
    }

    /// Build a bounded-range fixture whose only activity is a long attach plus one
    /// input tick of `kind` at 10:30, used to compare `active` vs `work_active`.
    fn data_with_single_tick(kind: HumanInteractionKind) -> StatsData {
        let mut d = data(Vec::new());
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.pane_sessions.insert("%1".into(), "main".into());
        d.activity_entries = vec![
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxAttach,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 10:00:00 UTC),
                ended_at: datetime!(2026-05-30 12:00:00 UTC),
            })),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind,
                pane: None,
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 10:29:59 UTC),
                ended_at: datetime!(2026-05-30 10:30:00 UTC),
            })),
        ];
        d
    }

    #[test]
    fn work_active_excludes_scroll_ticks() {
        // A keypress tick feeds both `active` and `work_active`; a scrollback tick
        // feeds `active` (engaged/watching) but not `work_active` (hands-on).
        let keypress = build_totals(&data_with_single_tick(HumanInteractionKind::TmuxInput));
        assert_eq!(keypress.active_secs, 361);
        assert_eq!(keypress.work_active_secs, 361);

        let scroll = build_totals(&data_with_single_tick(HumanInteractionKind::TmuxScroll));
        // Scroll still counts as engaged time...
        assert_eq!(scroll.active_secs, 361);
        // ...but is excluded from hands-on work.
        assert_eq!(scroll.work_active_secs, 0);
    }

    #[test]
    fn count_tmux_input_off_drops_tick_active() {
        // With `count_tmux_input = false`, tmux keypress/scroll ticks seed no ACTIVE
        // windows at all — only prompts and thinking would. The fixture's only
        // anchor is a single tick plus an idle attach, so ACTIVE collapses to zero.
        // This is the opt-out for `mouse on` sessions, where tmux reports mouse
        // motion/wheel as indistinguishable from a keypress behind client_activity.
        for kind in [
            HumanInteractionKind::TmuxInput,
            HumanInteractionKind::TmuxScroll,
        ] {
            let mut d = data_with_single_tick(kind);
            d.count_tmux_input = false;
            let totals = build_totals(&d);
            assert_eq!(totals.active_secs, 0, "kind={kind:?}");
            assert_eq!(totals.work_active_secs, 0, "kind={kind:?}");
        }
    }

    #[test]
    fn work_active_is_subset_of_same_last_touch_active_attribution() {
        let mut prompt_a = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/a"),
            "do a thing",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        prompt_a.tmux_session = Some("A".into());
        let mut d = data(vec![prompt_a]);
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.activity_entries = vec![
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$a",
                "A",
                datetime!(2026-05-30 10:00:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$b",
                "B",
                datetime!(2026-05-30 10:00:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxScroll,
                pane: None,
                session_id: Some("$b".into()),
                session_name: Some("B".into()),
                started_at: datetime!(2026-05-30 10:30:59 UTC),
                ended_at: datetime!(2026-05-30 10:31:00 UTC),
            })),
        ];

        let totals = build_totals(&d);
        assert_eq!(totals.active_secs, 420);
        assert_eq!(totals.work_active_secs, 59);

        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);
        let row = |key: &str| rows.iter().find(|r| r.key == key).unwrap();
        assert_eq!(row("A").active_secs, 59);
        assert_eq!(row("A").work_active_secs, 59);
        assert_eq!(row("B").active_secs, 361);
        assert_eq!(row("B").work_active_secs, 0);
        assert!(rows
            .iter()
            .all(|row| row.work_active_secs <= row.active_secs));
    }

    #[test]
    fn active_tick_uses_shorter_timeout_than_prompts() {
        // With a 90s tick timeout, the same keypress tick's window shrinks from
        // [10:28:59, 10:35:00] (361s) to [10:28:59, 10:31:30] (151s) — sparse
        // scrolling can no longer chain into long active spans.
        let mut d = data_with_single_tick(HumanInteractionKind::TmuxInput);
        d.active_tick_timeout = time::Duration::seconds(90);

        let totals = build_totals(&d);

        // 10:28:59 → 10:31:30 inclusive of the boundary second = 151s.
        assert_eq!(totals.active_secs, 151);
        assert_eq!(totals.work_active_secs, 151);
    }

    #[test]
    fn active_clips_prompt_padding_to_human_presence() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "do a thing",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        p.tmux_session = Some("main".into());
        let mut d = data(vec![p]);
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.activity_entries = vec![ActivityEntry::SessionForeground(
            SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-05-30 10:29:00 UTC),
                datetime!(2026-05-30 10:31:00 UTC),
            ),
        )];

        let totals = build_totals(&d);
        let row = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false)
            .into_iter()
            .find(|row| row.key == "main")
            .unwrap();

        assert_eq!(totals.foreground_secs, 120);
        assert_eq!(totals.human_secs, 120);
        assert_eq!(totals.active_secs, 120);
        assert_eq!(row.active_secs, 120);
    }

    #[test]
    fn active_omits_prompt_padding_without_presence() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "do a thing",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        p.tmux_session = Some("main".into());
        let mut d = data(vec![p]);
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };

        let totals = build_totals(&d);

        assert_eq!(totals.human_secs, 0);
        assert_eq!(totals.active_secs, 0);
    }

    #[test]
    fn active_omits_prompt_padding_with_watch_only_presence() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "do a thing",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        p.tmux_session = Some("main".into());
        let mut d = data(vec![p]);
        d.range = TimeRange {
            label: "bounded".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.activity_entries = vec![ActivityEntry::HumanInteraction(HumanInteractionEntry::new(
            HumanInteractionInput {
                kind: HumanInteractionKind::MuxaWatch,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 10:00:00 UTC),
                ended_at: datetime!(2026-05-30 12:00:00 UTC),
            },
        ))];

        let totals = build_totals(&d);

        assert_eq!(totals.human_secs, 7_200);
        assert_eq!(totals.active_secs, 0);
        assert_eq!(totals.work_active_secs, 0);
    }

    #[test]
    fn active_dedups_overlapping_sessions_by_last_touch() {
        // Two sessions whose padded windows overlap. A human does one thing at a
        // time, so the overlap is counted once (total = union) and attributed to
        // the most recently touched session (last-touch).
        let mut a = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/a"),
            "x",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        a.tmux_session = Some("A".into());
        let mut b = prompt(
            AgentKind::Codex,
            "agent-b",
            "%2",
            Some("/home/june/b"),
            "y",
            datetime!(2026-05-30 10:33:00 UTC),
        );
        b.tmux_session = Some("B".into());
        let mut d = data(vec![a, b]);
        d.range = TimeRange {
            label: "win".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.activity_entries = vec![
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$a",
                "A",
                datetime!(2026-05-30 10:00:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$b",
                "B",
                datetime!(2026-05-30 10:00:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
        ];

        // Windows: A [10:29,10:35], B [10:32,10:38]. Union = 540s (not 360+360).
        let totals = build_totals(&d);
        assert_eq!(totals.active_secs, 540);

        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);
        let secs = |key: &str| {
            rows.iter()
                .find(|r| r.key == key)
                .map_or(0, |r| r.active_secs)
        };
        // Last touch: B started later (10:32), so it owns the [10:32,10:35] overlap.
        assert_eq!(secs("A"), 180); // [10:29, 10:32)
        assert_eq!(secs("B"), 360); // [10:32, 10:38)
        assert_eq!(secs("A") + secs("B"), totals.active_secs);
    }

    #[test]
    fn last_touch_uses_unclipped_anchor_at_range_boundary() {
        // Two prompts within `active_lookback` of the range start, so both
        // windows clamp to the same boundary (10:30:00). Recency must follow the
        // *unclipped* prompt time, not the clipped span start — otherwise the
        // overlap is mis-attributed by input order (history is newest-first).
        // Prompts are passed newest-first to expose that ordering bug.
        let mut newer = prompt(
            AgentKind::Codex,
            "agent-b",
            "%2",
            Some("/home/june/b"),
            "y",
            datetime!(2026-05-30 10:30:40 UTC),
        );
        newer.tmux_session = Some("B".into());
        let mut older = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/a"),
            "x",
            datetime!(2026-05-30 10:30:10 UTC),
        );
        older.tmux_session = Some("A".into());
        let mut d = data(vec![newer, older]); // newest-first, as history returns
        d.range = TimeRange {
            label: "win".into(),
            since_at: Some(datetime!(2026-05-30 10:30:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.activity_entries = vec![
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$a",
                "A",
                datetime!(2026-05-30 10:30:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$b",
                "B",
                datetime!(2026-05-30 10:30:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
        ];

        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);
        let secs = |key: &str| {
            rows.iter()
                .find(|r| r.key == key)
                .map_or(0, |r| r.active_secs)
        };
        // B's window [10:30:00, 10:35:40] (anchor 10:30:40) shadows A's
        // [10:30:00, 10:35:10] (anchor 10:30:10), so B owns all 340s.
        assert_eq!(secs("B"), 340);
        assert_eq!(secs("A"), 0);
        assert_eq!(build_totals(&d).active_secs, 340);
    }

    #[test]
    fn last_touch_orders_subsecond_touches() {
        // Two prompts in the same unix second; their windows clamp to identical
        // whole-second spans, so only sub-second recency separates them. The
        // anchor keeps nanosecond precision, so the later prompt wins even though
        // history hands them over newest-first (which would otherwise tiebreak
        // to the older one by index).
        let mut newer = prompt(
            AgentKind::Codex,
            "agent-b",
            "%2",
            Some("/home/june/b"),
            "y",
            datetime!(2026-05-30 10:30:10.700 UTC),
        );
        newer.tmux_session = Some("B".into());
        let mut older = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/a"),
            "x",
            datetime!(2026-05-30 10:30:10.100 UTC),
        );
        older.tmux_session = Some("A".into());
        let mut d = data(vec![newer, older]); // newest-first
        d.range = TimeRange {
            label: "win".into(),
            since_at: Some(datetime!(2026-05-30 10:30:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.activity_entries = vec![
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$a",
                "A",
                datetime!(2026-05-30 10:30:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$b",
                "B",
                datetime!(2026-05-30 10:30:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            )),
        ];

        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);
        let secs = |key: &str| {
            rows.iter()
                .find(|r| r.key == key)
                .map_or(0, |r| r.active_secs)
        };
        // Both span [10:30:00, 10:35:10] = 310s; the later (sub-second) prompt wins.
        assert_eq!(secs("B"), 310);
        assert_eq!(secs("A"), 0);
    }

    #[test]
    fn active_window_respects_configured_padding() {
        // The window size is driven by `[stats]` config, not a hardcoded const.
        let p = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/a"),
            "x",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        let mut d = data(vec![p]);
        d.range = TimeRange {
            label: "win".into(),
            since_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };
        d.pane_sessions.insert("%1".into(), "A".into());
        d.activity_entries = vec![ActivityEntry::SessionForeground(
            SessionForegroundEntry::new(
                "$a",
                "A",
                datetime!(2026-05-30 10:00:00 UTC),
                datetime!(2026-05-30 12:00:00 UTC),
            ),
        )];
        d.active_lookback = time::Duration::seconds(0);
        d.active_timeout = time::Duration::seconds(120);

        // Window = [10:30:00, 10:32:00] = 120s.
        assert_eq!(build_totals(&d).active_secs, 120);
    }

    #[test]
    fn active_tmux_input_buckets_by_tick_day_not_padded_window() {
        // A tmux input tick in the last 5 minutes of a day: its padded ACTIVE
        // window ends after midnight, but per-day attribution must follow the
        // tick time, landing on the tick's day — not rolling into the next.
        let mut d = data(Vec::new());
        d.range = TimeRange {
            label: "two days".into(),
            since_at: Some(datetime!(2026-05-29 00:00:00 UTC)),
            until_at: Some(datetime!(2026-05-31 00:00:00 UTC)),
        };
        d.activity_entries = vec![ActivityEntry::HumanInteraction(HumanInteractionEntry::new(
            HumanInteractionInput {
                kind: HumanInteractionKind::TmuxInput,
                pane: None,
                session_id: Some("$1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-29 23:57:59 UTC),
                ended_at: datetime!(2026-05-29 23:58:00 UTC),
            },
        ))];
        d.activity_entries.push(ActivityEntry::SessionForeground(
            SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-05-29 23:50:00 UTC),
                datetime!(2026-05-29 23:59:00 UTC),
            ),
        ));

        // Day buckets follow the viewer's local calendar day, so derive the
        // expected keys from the anchor via the same `format_day` rather than
        // hard-coding UTC dates — this keeps the test valid in any timezone.
        let anchor = datetime!(2026-05-29 23:58:00 UTC);
        let tick_day = format_day(anchor);
        let next_day = format_day(anchor + time::Duration::days(1));

        let rows = build_rows(&d, GroupBy::Day, 0, SortKey::Prompts, false);
        let on_tick_day = rows.iter().find(|r| r.key == tick_day);
        let on_next_day = rows.iter().find(|r| r.key == next_day);

        assert!(
            on_tick_day.is_some_and(|r| r.active_secs > 0),
            "the tick's ACTIVE window must land on its own day"
        );
        assert!(
            on_next_day.is_none_or(|r| r.active_secs == 0),
            "the padded window must not roll ACTIVE into the next day"
        );
    }

    #[test]
    fn time_graph_uses_hour_buckets_for_day_range() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "do a thing",
            datetime!(2026-05-30 10:30:00 UTC),
        );
        p.tmux_session = Some("main".into());
        let mut d = data(vec![p]);
        d.range = TimeRange {
            label: "day".into(),
            since_at: Some(datetime!(2026-05-30 00:00:00 UTC)),
            until_at: Some(datetime!(2026-05-31 00:00:00 UTC)),
        };
        d.activity_entries = vec![ActivityEntry::SessionForeground(
            SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-05-30 10:00:00 UTC),
                datetime!(2026-05-30 11:00:00 UTC),
            ),
        )];

        let totals = build_totals(&d);
        let graph = build_time_graph(&d).expect("day range should produce a graph");

        assert_eq!(graph.bucket, "hour");
        assert_eq!(graph.total_secs, totals.work_active_secs);
        assert!(graph
            .buckets
            .iter()
            .any(|bucket| bucket.work_active_secs == 360));
    }

    #[test]
    fn time_graph_uses_day_buckets_for_week_range() {
        let mut d = data(Vec::new());
        d.range = TimeRange {
            label: "week".into(),
            since_at: Some(datetime!(2026-05-23 12:00:00 UTC)),
            until_at: Some(datetime!(2026-05-30 12:00:00 UTC)),
        };

        let graph = build_time_graph(&d).expect("bounded week should produce a graph");

        assert_eq!(graph.bucket, "day");
    }

    #[test]
    fn time_graph_keeps_active_and_work_active_separate() {
        let d = data_with_single_tick(HumanInteractionKind::TmuxScroll);
        let graph = build_time_graph(&d).expect("scroll activity should produce a graph");

        assert_eq!(graph.total_secs, 0);
        assert_eq!(
            graph
                .buckets
                .iter()
                .map(|bucket| bucket.active_secs)
                .sum::<u64>(),
            361
        );
        assert_eq!(
            graph
                .buckets
                .iter()
                .map(|bucket| bucket.work_active_secs)
                .sum::<u64>(),
            0
        );
    }

    #[test]
    fn time_graph_render_clamps_to_narrow_width() {
        let graph = TimeGraph {
            metric: "work_active".into(),
            bucket: "hour".into(),
            total_secs: 3_600,
            total: "1h".into(),
            max_secs: 3_600,
            max: "1h".into(),
            buckets: vec![TimeGraphBucket {
                label: "06-24 10:00".into(),
                started_at: "2026-06-24T10:00:00Z".into(),
                ended_at: "2026-06-24T11:00:00Z".into(),
                active_secs: 3_600,
                active: "1h".into(),
                work_active_secs: 3_600,
                work_active: "1h".into(),
            }],
        };

        let rendered = render_time_graph(&graph, 40, CliTheme::plain());

        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 40,
                "line exceeded graph width: {line:?}"
            );
        }
    }

    #[test]
    fn stats_document_omits_time_graph_by_default() {
        let d = data(Vec::new());

        let doc = build_document(&d, GroupBy::Day, 0, SortKey::Prompts, false, false);

        assert!(doc.time_graph.is_none());
    }

    #[test]
    fn scope_exclusions_remove_matching_source_data() {
        let mut kept_prompt = prompt(
            AgentKind::Codex,
            "agent-main",
            "%1",
            Some("/home/june/main"),
            "ship",
            datetime!(2026-05-30 11:00:00 UTC),
        );
        kept_prompt.tmux_session = Some("main".into());
        let mut excluded_prompt = prompt(
            AgentKind::Codex,
            "agent-monitor",
            "%monitor",
            Some("/home/june/monitor"),
            "watch",
            datetime!(2026-05-30 11:01:00 UTC),
        );
        excluded_prompt.tmux_session = Some("monitoring".into());
        let mut d = data(vec![kept_prompt, excluded_prompt]);
        d.pane_sessions.insert("%1".into(), "main".into());
        d.pane_sessions
            .insert("%monitor".into(), "monitoring".into());
        d.activity_entries = vec![
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-05-30 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: None,
                from: AgentState::Working,
                to: AgentState::Idle,
                state_entered_at: Some(datetime!(2026-05-30 11:00:00 UTC)),
            })),
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-05-30 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-monitor".into(),
                pane: Some("%monitor".into()),
                session_name: Some("monitoring".into()),
                cwd: None,
                from: AgentState::Working,
                to: AgentState::Idle,
                state_entered_at: Some(datetime!(2026-05-30 11:00:00 UTC)),
            })),
        ];
        d.agents.push(live_agent(
            AgentState::Working,
            datetime!(2026-05-30 11:00:00 UTC),
            Some("/home/june/monitor"),
        ));
        d.agents[0].session_id = "agent-monitor".into();
        d.agents[0].pane = Some("%monitor".into());
        d.activities = vec![
            SessionActivity {
                session_id: "$main".into(),
                name: "main".into(),
                attached_clients: 1,
                total_attached_secs: 60,
                attached_since: None,
                last_seen_at: datetime!(2026-05-30 11:00:00 UTC),
            },
            SessionActivity {
                session_id: "$monitor".into(),
                name: "monitoring".into(),
                attached_clients: 1,
                total_attached_secs: 60,
                attached_since: None,
                last_seen_at: datetime!(2026-05-30 11:00:00 UTC),
            },
        ];

        apply_exclusions(
            &mut d,
            &ScopeExclusions::new(vec!["%monitor*".into()], vec!["monitor*".into()]),
        );

        assert_eq!(d.prompts.len(), 1);
        assert_eq!(d.prompts[0].tmux_session.as_deref(), Some("main"));
        assert_eq!(d.activity_entries.len(), 1);
        assert!(matches!(
            &d.activity_entries[0],
            ActivityEntry::StateTransition(entry) if entry.session_name.as_deref() == Some("main")
        ));
        assert!(d.agents.is_empty());
        assert_eq!(d.activities.len(), 1);
        assert_eq!(d.activities[0].name, "main");
    }

    #[test]
    fn build_rows_groups_by_project() {
        let d = data(vec![
            prompt(
                AgentKind::ClaudeCode,
                "a",
                "%1",
                Some("/home/june/muxa"),
                "hello world",
                datetime!(2026-05-30 11:00:00 UTC),
            ),
            prompt(
                AgentKind::Codex,
                "b",
                "%2",
                Some("/home/june/muxa"),
                "ship it",
                datetime!(2026-05-30 11:01:00 UTC),
            ),
            prompt(
                AgentKind::GeminiCli,
                "c",
                "%3",
                Some("/home/june/other"),
                "x",
                datetime!(2026-05-30 11:02:00 UTC),
            ),
        ]);

        let rows = build_rows(&d, GroupBy::Project, 0, SortKey::Prompts, false);
        assert_eq!(rows[0].key, "muxa");
        assert_eq!(rows[0].prompts, 2);
        assert_eq!(rows[0].agent_sessions, 2);
        assert_eq!(rows[1].key, "other");
        assert_eq!(rows[1].prompts, 1);
    }

    fn sort_fixture() -> StatsData {
        data(vec![
            prompt(
                AgentKind::ClaudeCode,
                "a",
                "%1",
                Some("/home/june/muxa"),
                "hello world",
                datetime!(2026-05-30 11:00:00 UTC),
            ),
            prompt(
                AgentKind::Codex,
                "b",
                "%2",
                Some("/home/june/muxa"),
                "ship it",
                datetime!(2026-05-30 11:01:00 UTC),
            ),
            prompt(
                AgentKind::GeminiCli,
                "c",
                "%3",
                Some("/home/june/other"),
                "x",
                datetime!(2026-05-30 11:02:00 UTC),
            ),
        ])
    }

    fn ordered_keys(rows: &[GroupRow]) -> Vec<&str> {
        rows.iter().map(|row| row.key.as_str()).collect()
    }

    #[test]
    fn sort_key_and_reverse_control_row_order() {
        let d = sort_fixture();

        // Default: highest prompt count first.
        let by_prompts = build_rows(&d, GroupBy::Project, 0, SortKey::Prompts, false);
        assert_eq!(ordered_keys(&by_prompts), ["muxa", "other"]);

        // --reverse flips the default direction.
        let reversed = build_rows(&d, GroupBy::Project, 0, SortKey::Prompts, true);
        assert_eq!(ordered_keys(&reversed), ["other", "muxa"]);

        // name sorts ascending by group key; --reverse makes it descending.
        let by_name = build_rows(&d, GroupBy::Project, 0, SortKey::Name, false);
        assert_eq!(ordered_keys(&by_name), ["muxa", "other"]);
        let by_name_rev = build_rows(&d, GroupBy::Project, 0, SortKey::Name, true);
        assert_eq!(ordered_keys(&by_name_rev), ["other", "muxa"]);
    }

    #[test]
    fn stats_table_appends_total_footer() {
        let d = sort_fixture();
        let doc = build_document(&d, GroupBy::Project, 0, SortKey::Prompts, false, false);
        let rendered = render_stats_table(&doc, 140, CliTheme::plain(), false);

        let lines: Vec<&str> = rendered.lines().collect();
        let total_idx = lines
            .iter()
            .position(|line| line.contains("TOTAL"))
            .expect("TOTAL footer row present");
        // The grand total is the last content row, ruled off like the header.
        assert_eq!(total_idx, lines.len() - 2);
        assert!(
            lines[total_idx - 1].starts_with('╞'),
            "expected a separator rule above TOTAL, got {:?}",
            lines[total_idx - 1]
        );
    }

    #[test]
    fn output_format_resolve_applies_shortcut_precedence() {
        // --json / --markdown override the base; neither set leaves the base.
        assert_eq!(
            OutputFormat::resolve(OutputFormat::Table, true, false),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::resolve(OutputFormat::Table, false, true),
            OutputFormat::Markdown
        );
        assert_eq!(
            OutputFormat::resolve(OutputFormat::Markdown, false, false),
            OutputFormat::Markdown
        );
        assert_eq!(
            OutputFormat::resolve(OutputFormat::Table, false, false),
            OutputFormat::Table
        );
    }

    #[test]
    fn build_rows_limits_after_sorting() {
        let d = data(vec![
            prompt(
                AgentKind::ClaudeCode,
                "a",
                "%1",
                Some("/p/a"),
                "one",
                datetime!(2026-05-30 11:00:00 UTC),
            ),
            prompt(
                AgentKind::ClaudeCode,
                "a",
                "%1",
                Some("/p/a"),
                "two",
                datetime!(2026-05-30 11:01:00 UTC),
            ),
            prompt(
                AgentKind::Codex,
                "b",
                "%2",
                Some("/p/b"),
                "three",
                datetime!(2026-05-30 11:02:00 UTC),
            ),
        ]);

        let rows = build_rows(&d, GroupBy::Project, 1, SortKey::Prompts, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "a");
    }

    #[test]
    fn totals_include_activity_ledger_durations() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-05-30 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%1".into()),
                session_name: Some("muxa-session".into()),
                cwd: Some("/home/june/muxa".into()),
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-05-30 11:00:00 UTC)),
            })),
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-05-30 11:20:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%1".into()),
                session_name: Some("muxa-session".into()),
                cwd: Some("/home/june/muxa".into()),
                from: AgentState::WaitingInput,
                to: AgentState::Working,
                state_entered_at: Some(datetime!(2026-05-30 11:10:00 UTC)),
            })),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-05-30 11:00:00 UTC),
                datetime!(2026-05-30 11:30:00 UTC),
            )),
        ];

        let totals = build_totals(&d);

        assert_eq!(totals.working_secs, 600);
        assert_eq!(totals.waiting_secs, 600);
        assert_eq!(totals.foreground_secs, 1_800);
        assert_eq!(totals.attention_events, 1);
    }

    #[test]
    fn thinking_counts_attention_overlap_with_human_presence() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-05-30 11:30:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: Some("/home/june/muxa".into()),
                from: AgentState::WaitingInput,
                to: AgentState::Working,
                state_entered_at: Some(datetime!(2026-05-30 11:10:00 UTC)),
            })),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-05-30 11:00:00 UTC),
                datetime!(2026-05-30 11:20:00 UTC),
            )),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaPromptInput,
                pane: Some("%1".into()),
                session_id: Some("$1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 11:18:00 UTC),
                ended_at: datetime!(2026-05-30 11:25:00 UTC),
            })),
        ];

        let totals = build_totals(&d);
        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);

        assert_eq!(totals.waiting_secs, 1_200);
        assert_eq!(totals.human_secs, 1_500);
        assert_eq!(totals.thinking_secs, 900);
        assert_eq!(rows[0].key, "main");
        assert_eq!(rows[0].thinking_secs, 900);
    }

    #[test]
    fn watch_open_does_not_count_as_thinking() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-05-30 11:20:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: Some("/home/june/muxa".into()),
                from: AgentState::Error,
                to: AgentState::Working,
                state_entered_at: Some(datetime!(2026-05-30 11:10:00 UTC)),
            })),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaWatch,
                pane: Some("%1".into()),
                session_id: Some("$1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-30 11:10:00 UTC),
                ended_at: datetime!(2026-05-30 11:20:00 UTC),
            })),
        ];

        let totals = build_totals(&d);

        assert_eq!(totals.error_secs, 600);
        assert_eq!(totals.human_secs, 600);
        assert_eq!(totals.thinking_secs, 0);
    }

    #[test]
    fn rows_group_activity_ledger_by_project() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![ActivityEntry::StateTransition(StateTransitionEntry::new(
            StateTransitionInput {
                at: datetime!(2026-05-30 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%1".into()),
                session_name: Some("muxa-session".into()),
                cwd: Some("/home/june/muxa".into()),
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-05-30 11:00:00 UTC)),
            },
        ))];

        let rows = build_rows(&d, GroupBy::Project, 0, SortKey::Prompts, false);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "muxa");
        assert_eq!(rows[0].working_secs, 600);
        assert_eq!(rows[0].attention_events, 1);
    }

    #[test]
    fn rows_group_prompts_by_stored_tmux_session() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-a",
            "%dead",
            Some("/home/june/muxa"),
            "hello",
            datetime!(2026-05-30 11:00:00 UTC),
        );
        p.tmux_session = Some("deleted-session-name".into());
        let d = data(vec![p]);

        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);

        assert_eq!(rows[0].key, "deleted-session-name");
    }

    #[test]
    fn rows_group_state_duration_by_stored_tmux_session() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![ActivityEntry::StateTransition(StateTransitionEntry::new(
            StateTransitionInput {
                at: datetime!(2026-05-30 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%dead".into()),
                session_name: Some("deleted-session-name".into()),
                cwd: Some("/home/june/muxa".into()),
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-05-30 11:00:00 UTC)),
            },
        ))];

        let rows = build_rows(&d, GroupBy::Session, 0, SortKey::Prompts, false);

        assert_eq!(rows[0].key, "deleted-session-name");
        assert_eq!(rows[0].working_secs, 600);
    }

    #[test]
    fn stats_table_verbose_compacts_without_wrapping_at_88_cols() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/muxa"),
            "hello",
            datetime!(2026-05-30 11:00:00 UTC),
        );
        p.tmux_session = Some("callabo-auto-label".into());
        let mut long = prompt(
            AgentKind::Codex,
            "agent-b",
            "%2",
            Some("/home/june/muxa"),
            "hello",
            datetime!(2026-05-30 11:01:00 UTC),
        );
        long.tmux_session = Some("9248e2a7-88f8-4229-ad96-eaf257accdfc".into());
        let d = data(vec![p, long]);
        let doc = build_document(&d, GroupBy::Session, 0, SortKey::Prompts, false, false);

        let rendered = render_stats_table(&doc, 88, CliTheme::plain(), true);

        assert!(rendered.contains("PRM"));
        assert!(!rendered.contains("TOK EST"));
        assert!(rendered.contains("callabo-auto-label"));
        assert!(rendered.contains("9248e2a7-88f8-4229"));
        assert!(rendered.contains("..."));
        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 88,
                "line exceeded compact table width: {line:?}"
            );
        }
    }

    #[test]
    fn stats_table_summary_layout_keeps_review_columns_when_wide() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/muxa"),
            "hello",
            datetime!(2026-05-30 11:00:00 UTC),
        );
        p.tmux_session = Some("callabo-auto-label".into());
        let d = data(vec![p]);
        let doc = build_document(&d, GroupBy::Session, 0, SortKey::Prompts, false, false);

        let rendered = render_stats_table(&doc, 140, CliTheme::plain(), false);

        assert!(rendered.contains("ACT"));
        assert!(rendered.contains("WACT"));
        assert!(rendered.contains("WORK"));
        assert!(rendered.contains("WAIT"));
        assert!(rendered.contains("BLK"));
        assert!(rendered.contains("PROMPTS"));
        assert!(rendered.contains("LAST"));
        assert!(!rendered.contains("ERR"));
        assert!(!rendered.contains("TMUX"));
        assert!(!rendered.contains("HUMAN"));
        assert!(!rendered.contains("THINK"));
        assert!(!rendered.contains("TOK EST"));
        assert!(!rendered.contains("AGENTS"));
        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 140,
                "line exceeded summary table width: {line:?}"
            );
        }
    }

    #[test]
    fn stats_table_verbose_layout_keeps_diagnostic_columns_when_wide() {
        let mut p = prompt(
            AgentKind::Codex,
            "agent-a",
            "%1",
            Some("/home/june/muxa"),
            "hello",
            datetime!(2026-05-30 11:00:00 UTC),
        );
        p.tmux_session = Some("callabo-auto-label".into());
        let d = data(vec![p]);
        let doc = build_document(&d, GroupBy::Session, 0, SortKey::Prompts, false, false);

        let rendered = render_stats_table(&doc, 140, CliTheme::plain(), true);

        assert!(rendered.contains("WORK"));
        assert!(rendered.contains("TOK EST"));
        assert!(rendered.contains("ACT"));
        assert!(rendered.contains("AGENTS"));
        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 140,
                "line exceeded verbose table width: {line:?}"
            );
        }
    }

    #[test]
    fn open_live_agent_state_counts_until_now() {
        let mut d = data(Vec::new());
        d.agents.push(live_agent(
            AgentState::Working,
            datetime!(2026-05-30 11:00:00 UTC),
            Some("/home/june/muxa"),
        ));

        let totals = build_totals(&d);
        let rows = build_rows(&d, GroupBy::Project, 0, SortKey::Prompts, false);

        assert_eq!(totals.working_secs, 3_600);
        assert_eq!(rows[0].key, "muxa");
        assert_eq!(rows[0].working_secs, 3_600);
    }

    #[test]
    fn format_duration_matches_watch_shape() {
        assert_eq!(format_duration(0), "-");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(3_660), "1h01m");
        assert_eq!(format_duration(90_000), "1d01h");
    }
}
