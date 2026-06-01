use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ContentArrangement, Table};
use muxa::event::AgentState;
use muxa::ipc::Client;
use muxa::{
    ActivityEntry, Agent, Config, HistoryEntry, SessionActivity, SessionForegroundEntry,
    StateTransitionEntry,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Time window to include: 24h, 7d, 4w, RFC3339 timestamp, or all.
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
}

#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// Time window to include: 24h, 7d, 4w, RFC3339 timestamp, or all.
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
        OutputFormat::Table => render_table(&doc),
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
struct StatsRange {
    label: String,
    since_at: Option<OffsetDateTime>,
}

impl StatsRange {
    fn includes(&self, at: OffsetDateTime) -> bool {
        self.since_at.is_none_or(|since| at >= since)
    }
}

#[derive(Debug)]
struct StatsData {
    now: OffsetDateTime,
    range: StatsRange,
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
    attention_events: usize,
    last_prompt_at: Option<OffsetDateTime>,
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
            rows.entry(key).or_default().foreground_secs += activity.effective_total_secs(data.now);
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
        GroupBy::Session => data
            .pane_sessions
            .get(&prompt.pane)
            .cloned()
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
            .pane
            .as_ref()
            .and_then(|pane| data.pane_sessions.get(pane))
            .cloned()
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

fn overlap_secs(data: &StatsData, started_at: OffsetDateTime, ended_at: OffsetDateTime) -> u64 {
    let start = data
        .range
        .since_at
        .map_or(started_at, |since| started_at.max(since));
    let end = ended_at.min(data.now);
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

fn parse_since(raw: &str, now: OffsetDateTime) -> Result<StatsRange> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(StatsRange {
            label: "all retained history".to_string(),
            since_at: None,
        });
    }
    if let Ok(at) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Ok(StatsRange {
            label: format!("since {trimmed}"),
            since_at: Some(at),
        });
    }
    if trimmed.is_empty() {
        bail!("--since must be a duration like 7d, an RFC3339 timestamp, or all");
    }

    let unit = trimmed
        .chars()
        .last()
        .context("--since must be a duration like 7d, an RFC3339 timestamp, or all")?;
    let number = &trimmed[..trimmed.len() - unit.len_utf8()];
    let amount: i64 = number
        .parse()
        .with_context(|| format!("invalid --since duration {trimmed:?}"))?;
    if amount <= 0 {
        bail!("--since duration must be greater than zero");
    }
    let duration = match unit {
        's' => time::Duration::seconds(amount),
        'm' => time::Duration::minutes(amount),
        'h' => time::Duration::hours(amount),
        'd' => time::Duration::days(amount),
        'w' => time::Duration::weeks(amount),
        _ => bail!("--since duration unit must be one of s, m, h, d, w"),
    };

    Ok(StatsRange {
        label: format!("last {trimmed}"),
        since_at: Some(now - duration),
    })
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
            "No activity ledger entries found yet; duration columns will fill as agents transition and tmux foreground intervals close.".to_string(),
        );
    } else if !has_session_foreground_ledger(data) && !data.activities.is_empty() {
        notes.push(
            "TMUX uses legacy cumulative session-activity.json totals until the first session foreground interval lands in activity.ndjson.".to_string(),
        );
    }
    notes
}

fn render_table(doc: &StatsDocument) {
    println!("muxa stats");
    println!("Range: {}", doc.range.label);
    if let Some(since_at) = doc.range.since_at.as_deref() {
        println!("Since: {since_at}");
    }
    println!(
        "Prompts: {} | token est: {} | agent sessions: {} | live agents: {} | work: {} | wait: {} | err: {} | tmux: {} | blocks: {}",
        doc.totals.prompts,
        doc.totals.token_estimate,
        doc.totals.agent_sessions,
        doc.totals.live_agents,
        doc.totals.working,
        doc.totals.waiting,
        doc.totals.error,
        doc.totals.foreground,
        doc.totals.attention_events
    );
    println!();

    if doc.rows.is_empty() {
        println!("no retained prompts, live agents, or tracked session activity in this view");
    } else {
        let mut table = Table::new();
        table
            .load_preset(UTF8_BORDERS_ONLY)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec![
                doc.group_by.to_ascii_uppercase(),
                "PROMPTS".to_string(),
                "WORK".to_string(),
                "WAIT".to_string(),
                "ERR".to_string(),
                "TMUX".to_string(),
                "BLOCK".to_string(),
                "TOK EST".to_string(),
                "WORDS".to_string(),
                "SESS".to_string(),
                "AGENTS".to_string(),
                "LAST".to_string(),
            ]);

        for row in &doc.rows {
            table.add_row(vec![
                Cell::new(&row.key),
                Cell::new(row.prompts),
                Cell::new(&row.working),
                Cell::new(&row.waiting),
                Cell::new(&row.error),
                Cell::new(&row.foreground),
                Cell::new(row.attention_events),
                Cell::new(row.token_estimate),
                Cell::new(row.words),
                Cell::new(row.agent_sessions),
                Cell::new(row.live_agents),
                Cell::new(&row.last_prompt_age),
            ]);
        }
        println!("{table}");
    }

    for note in &doc.notes {
        println!("note: {note}");
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

    out.push_str("| Group | Prompts | Work | Wait | Error | TMUX | Block | Tok est | Words | Sessions | Agents | Last |\n");
    out.push_str(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n",
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
    at.format(&Rfc3339).unwrap_or_else(|_| at.to_string())
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
    use muxa::StateTransitionInput;
    use time::macros::datetime;

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
            range: StatsRange {
                label: "last 7d".into(),
                since_at: Some(datetime!(2026-05-23 12:00:00 UTC)),
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
    fn rows_group_activity_ledger_by_project() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![ActivityEntry::StateTransition(StateTransitionEntry::new(
            StateTransitionInput {
                at: datetime!(2026-05-30 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-a".into(),
                pane: Some("%1".into()),
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
