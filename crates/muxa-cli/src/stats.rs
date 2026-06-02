use anyhow::{Context, Result};
use clap::ValueEnum;
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{ColumnConstraint, ContentArrangement, Table, Width};
use muxa::event::AgentState;
use muxa::ipc::Client;
use muxa::{
    ActivityEntry, Agent, Config, HistoryEntry, HumanInteractionKind, SessionActivity,
    SessionForegroundEntry, StateTransitionEntry,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use time::OffsetDateTime;

use crate::theme::{self, CliTheme, TableTone, ThemeArg};
use crate::time_range::TimeRange;
use crate::{terminal_width, truncate_cell, use_colors};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Time window to include: today, yesterday, week, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "7d")]
    since: String,

    /// Dimension used for the row breakdown.
    #[arg(long, value_enum, default_value_t = GroupBy::Day)]
    group_by: GroupBy,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    /// Maximum rows to print. Set 0 for all rows.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// One-shot visual theme override for table output.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,
}

impl Args {
    #[cfg(test)]
    pub(crate) fn theme(&self) -> Option<ThemeArg> {
        self.theme
    }
}

#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// Time window to include: today, yesterday, week, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "7d")]
    since: String,

    /// Maximum rows per report section. Set 0 for all rows.
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Table,
    Json,
    Markdown,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum GroupBy {
    Day,
    Project,
    Agent,
    Session,
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
    let data = load_data(client, cfg, &args.since).await?;
    let doc = build_document(&data, args.group_by, args.limit);
    match args.format {
        OutputFormat::Table => render_table(&doc, theme::for_config(cfg, args.theme, use_colors())),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&doc)?),
        OutputFormat::Markdown => print!("{}", render_markdown_stats(&doc)),
    }
    Ok(())
}

pub async fn run_report(client: &Client, cfg: &Config, args: ReportArgs) -> Result<()> {
    let data = load_data(client, cfg, &args.since).await?;
    let docs = [
        build_document(&data, GroupBy::Day, args.limit),
        build_document(&data, GroupBy::Project, args.limit),
        build_document(&data, GroupBy::Agent, args.limit),
        build_document(&data, GroupBy::Session, args.limit),
    ];
    print!("{}", render_markdown_report(&docs));
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
}

#[derive(Debug, Serialize)]
struct StatsDocument {
    generated_at: String,
    range: RangeDocument,
    group_by: String,
    totals: Totals,
    rows: Vec<GroupRow>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RangeDocument {
    label: String,
    since_at: Option<String>,
    until_at: Option<String>,
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
    attention_events: usize,
    last_prompt_at: Option<String>,
    last_prompt_age: String,
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
    attention_events: usize,
    last_prompt_at: Option<OffsetDateTime>,
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
}

async fn load_data(client: &Client, cfg: &Config, since: &str) -> Result<StatsData> {
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

    Ok(StatsData {
        now,
        range,
        prompts,
        activity_entries,
        agents,
        activities,
        pane_sessions,
        project_by_pane,
        project_by_agent_session,
    })
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

fn build_document(data: &StatsData, group_by: GroupBy, limit: usize) -> StatsDocument {
    let rows = build_rows(data, group_by, limit);
    StatsDocument {
        generated_at: format_rfc3339(data.now),
        range: RangeDocument {
            label: data.range.label.clone(),
            since_at: data.range.since_at.map(format_rfc3339),
            until_at: data.range.until_at.map(format_rfc3339),
        },
        group_by: group_by.as_str().to_string(),
        totals: build_totals(data),
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
    human_secs = sum_merged_scoped_intervals(&human_presence_intervals(data, false));
    if !has_session_foreground_ledger {
        human_secs = human_secs.saturating_add(legacy_foreground_secs(data));
    }
    let thinking_secs = thinking_secs_total(data);

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
        attention_events,
        last_prompt_at: last_prompt_at.map(format_rfc3339),
        last_prompt_age: last_prompt_at
            .map_or_else(|| "-".to_string(), |at| relative_time(data.now, at)),
    }
}

fn build_rows(data: &StatsData, group_by: GroupBy, limit: usize) -> Vec<GroupRow> {
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
    rows.sort_by(|(a_key, a), (b_key, b)| {
        b.prompts
            .cmp(&a.prompts)
            .then_with(|| b.foreground_secs.cmp(&a.foreground_secs))
            .then_with(|| b.last_prompt_at.cmp(&a.last_prompt_at))
            .then_with(|| a_key.cmp(b_key))
    });
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
            attention_events: acc.attention_events,
            last_prompt_at: acc.last_prompt_at.map(format_rfc3339),
            last_prompt_age: acc
                .last_prompt_at
                .map_or_else(|| "-".to_string(), |at| relative_time(data.now, at)),
        })
        .collect()
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
    for interval in human_presence_intervals(data, false) {
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
    let presences = human_presence_intervals(data, true);
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

fn human_presence_intervals(data: &StatsData, thinking_only: bool) -> Vec<ScopedInterval> {
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
                if thinking_only && !human_interaction_counts_for_thinking(entry.kind) {
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
            intervals.push(AttentionInterval {
                interval,
                group_key: state_transition_group_key(data, entry, group_by),
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
            intervals.push(AttentionInterval {
                interval,
                group_key: agent_group_key(data, agent, group_by),
            });
        }
    }
    intervals
}

fn thinking_secs_total(data: &StatsData) -> u64 {
    let presences = human_presence_intervals(data, true);
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
    notes
}

fn render_table(doc: &StatsDocument, theme: CliTheme) {
    println!("muxa stats");
    println!("Range: {}", doc.range.label);
    if let Some(since_at) = doc.range.since_at.as_deref() {
        println!("Since: {since_at}");
    }
    if let Some(until_at) = doc.range.until_at.as_deref() {
        println!("Until: {until_at}");
    }
    println!(
        "Prompts: {} | token est: {} | agent sessions: {} | live agents: {} | work: {} | wait: {} | err: {} | tmux: {} | human: {} | think: {} | blocks: {}",
        doc.totals.prompts,
        doc.totals.token_estimate,
        doc.totals.agent_sessions,
        doc.totals.live_agents,
        doc.totals.working,
        doc.totals.waiting,
        doc.totals.error,
        doc.totals.foreground,
        doc.totals.human,
        doc.totals.thinking,
        doc.totals.attention_events
    );
    println!();

    if doc.rows.is_empty() {
        println!("no retained prompts, live agents, or tracked session activity in this view");
    } else {
        let terminal_width = terminal_width();
        println!("{}", render_stats_table(doc, terminal_width, theme));
        if stats_table_layout(terminal_width) != StatsTableLayout::Full {
            println!(
                "note: Table compacted for terminal width; use --format json or --format markdown for every column."
            );
        }
    }

    for note in &doc.notes {
        println!("note: {note}");
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
    AttentionEvents,
    TokenEstimate,
    Words,
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
        header: "WORDS",
        width: 6,
        value: StatsColumn::Words,
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

fn render_stats_table(doc: &StatsDocument, terminal_width: usize, theme: CliTheme) -> String {
    let layout = stats_table_layout(terminal_width);
    let columns = stats_table_columns(layout);
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

    format!("{table}")
}

fn stats_column_tone(column: StatsColumn) -> TableTone {
    match column {
        StatsColumn::Prompts
        | StatsColumn::TokenEstimate
        | StatsColumn::Words
        | StatsColumn::AgentSessions => TableTone::Accent,
        StatsColumn::Working | StatsColumn::LiveAgents => TableTone::Good,
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

fn stats_table_columns(layout: StatsTableLayout) -> &'static [StatsTableColumn] {
    match layout {
        StatsTableLayout::Full => FULL_STATS_COLUMNS,
        StatsTableLayout::Compact => COMPACT_STATS_COLUMNS,
        StatsTableLayout::Minimal => MINIMAL_STATS_COLUMNS,
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
        StatsColumn::AttentionEvents => row.attention_events.to_string(),
        StatsColumn::TokenEstimate => row.token_estimate.to_string(),
        StatsColumn::Words => row.words.to_string(),
        StatsColumn::AgentSessions => row.agent_sessions.to_string(),
        StatsColumn::LiveAgents => row.live_agents.to_string(),
        StatsColumn::LastPromptAge => row.last_prompt_age.clone(),
    }
}

fn render_markdown_stats(doc: &StatsDocument) -> String {
    let mut out = String::new();
    push_markdown_overview(&mut out, "muxa stats", doc);
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
    push_metric(out, "Generated", &doc.generated_at);
    push_metric(out, "Range", &doc.range.label);
    if let Some(since_at) = doc.range.since_at.as_deref() {
        push_metric(out, "Since", since_at);
    }
    if let Some(until_at) = doc.range.until_at.as_deref() {
        push_metric(out, "Until", until_at);
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

    out.push_str("| Group | Prompts | Work | Wait | Error | TMUX | Human | Think | Block | Tok est | Words | Sessions | Agents | Last |\n");
    out.push_str(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n",
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
    at.format(time::macros::format_description!("[year]-[month]-[day]"))
        .unwrap_or_else(|_| at.date().to_string())
}

fn format_rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
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
            kind: AgentKind::Codex,
            session_id: "agent-live".into(),
            pane: Some("%1".into()),
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
        }
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

        let rows = build_rows(&d, GroupBy::Project, 0);
        assert_eq!(rows[0].key, "muxa");
        assert_eq!(rows[0].prompts, 2);
        assert_eq!(rows[0].agent_sessions, 2);
        assert_eq!(rows[1].key, "other");
        assert_eq!(rows[1].prompts, 1);
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

        let rows = build_rows(&d, GroupBy::Project, 1);
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
        let rows = build_rows(&d, GroupBy::Session, 0);

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

        let rows = build_rows(&d, GroupBy::Project, 0);

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

        let rows = build_rows(&d, GroupBy::Session, 0);

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

        let rows = build_rows(&d, GroupBy::Session, 0);

        assert_eq!(rows[0].key, "deleted-session-name");
        assert_eq!(rows[0].working_secs, 600);
    }

    #[test]
    fn stats_table_compacts_without_wrapping_at_88_cols() {
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
        let doc = build_document(&d, GroupBy::Session, 0);

        let rendered = render_stats_table(&doc, 88, CliTheme::plain());

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
    fn stats_table_full_layout_keeps_all_columns_when_wide() {
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
        let doc = build_document(&d, GroupBy::Session, 0);

        let rendered = render_stats_table(&doc, 140, CliTheme::plain());

        assert!(rendered.contains("TOK EST"));
        assert!(rendered.contains("WORDS"));
        assert!(rendered.contains("AGENTS"));
        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 140,
                "line exceeded full table width: {line:?}"
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
        let rows = build_rows(&d, GroupBy::Project, 0);

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
