use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ContentArrangement, Table};
use muxa::event::AgentState;
use muxa::ipc::Client;
use muxa::{Agent, Config, HistoryEntry, SessionActivity};
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
    foreground_secs: u64,
    foreground: String,
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
    foreground_secs: u64,
    foreground: String,
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
    foreground_secs: u64,
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
        agents,
        activities,
        pane_sessions,
        project_by_pane,
        project_by_agent_session,
    })
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

    for prompt in &data.prompts {
        let metrics = prompt_metrics(prompt);
        prompt_chars += metrics.chars;
        words += metrics.words;
        token_estimate += metrics.token_estimate;
        agent_sessions.insert(prompt.session_id.clone());
        update_max_time(&mut last_prompt_at, prompt.at);
    }

    let foreground_secs = data
        .activities
        .iter()
        .map(|activity| activity.effective_total_secs(data.now))
        .sum();

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
        foreground_secs,
        foreground: format_duration(foreground_secs),
        last_prompt_at: last_prompt_at.map(format_rfc3339),
        last_prompt_age: last_prompt_at
            .map_or_else(|| "-".to_string(), |at| relative_time(data.now, at)),
    }
}

fn build_rows(data: &StatsData, group_by: GroupBy, limit: usize) -> Vec<GroupRow> {
    let mut rows = BTreeMap::<String, GroupAccumulator>::new();

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

    for agent in &data.agents {
        if agent.state == AgentState::Stopped || !data.range.includes(agent.last_activity_at) {
            continue;
        }
        let key = agent_group_key(data, agent, group_by);
        rows.entry(key).or_default().live_agents += 1;
    }

    if group_by == GroupBy::Session {
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
            foreground_secs: acc.foreground_secs,
            foreground: format_duration(acc.foreground_secs),
            last_prompt_at: acc.last_prompt_at.map(format_rfc3339),
            last_prompt_age: acc
                .last_prompt_at
                .map_or_else(|| "-".to_string(), |at| relative_time(data.now, at)),
        })
        .collect()
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
    if !data.activities.is_empty() {
        notes.push(
            "DUR is cumulative tmux foreground time from session-activity.json; it is not yet windowed by --since.".to_string(),
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
        "Prompts: {} | token est: {} | agent sessions: {} | live agents: {} | DUR: {}",
        doc.totals.prompts,
        doc.totals.token_estimate,
        doc.totals.agent_sessions,
        doc.totals.live_agents,
        doc.totals.foreground
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
                "TOK EST".to_string(),
                "WORDS".to_string(),
                "SESS".to_string(),
                "AGENTS".to_string(),
                "DUR".to_string(),
                "LAST".to_string(),
            ]);

        for row in &doc.rows {
            table.add_row(vec![
                Cell::new(&row.key),
                Cell::new(row.prompts),
                Cell::new(row.token_estimate),
                Cell::new(row.words),
                Cell::new(row.agent_sessions),
                Cell::new(row.live_agents),
                Cell::new(&row.foreground),
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
    push_metric(out, "DUR", &doc.totals.foreground);
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

    out.push_str("| Group | Prompts | Tok est | Words | Sessions | Agents | DUR | Last |\n");
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n");
    for row in rows {
        out.push_str("| ");
        out.push_str(&escape_markdown_cell(&row.key));
        out.push_str(" | ");
        out.push_str(&row.prompts.to_string());
        out.push_str(" | ");
        out.push_str(&row.token_estimate.to_string());
        out.push_str(" | ");
        out.push_str(&row.words.to_string());
        out.push_str(" | ");
        out.push_str(&row.agent_sessions.to_string());
        out.push_str(" | ");
        out.push_str(&row.live_agents.to_string());
        out.push_str(" | ");
        out.push_str(&escape_markdown_cell(&row.foreground));
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

    fn data(prompts: Vec<HistoryEntry>) -> StatsData {
        StatsData {
            now: datetime!(2026-05-30 12:00:00 UTC),
            range: StatsRange {
                label: "last 7d".into(),
                since_at: Some(datetime!(2026-05-23 12:00:00 UTC)),
            },
            prompts,
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
    fn format_duration_matches_watch_shape() {
        assert_eq!(format_duration(0), "-");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(3_660), "1h01m");
        assert_eq!(format_duration(90_000), "1d01h");
    }
}
