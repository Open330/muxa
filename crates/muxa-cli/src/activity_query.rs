use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{ColumnConstraint, ContentArrangement, Table, Width};
use muxa::{ActivityEntry, Config, HumanInteractionKind};
use serde::Serialize;
use time::OffsetDateTime;

use crate::theme::{self, CliTheme, TableTone, ThemeArg};
use crate::time_range::TimeRange;
use crate::{terminal_width, truncate_cell, use_colors};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Time window to include: today, yesterday, week, last-week, 24h, 7d, RFC3339 timestamp, or all.
    #[arg(long, default_value = "7d")]
    since: String,

    /// Filter by ledger entry type.
    #[arg(long = "type", value_enum)]
    entry_type: Option<EntryType>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    /// Maximum rows to print. Set 0 for all rows.
    #[arg(long, default_value_t = 50)]
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

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum EntryType {
    #[value(alias = "state")]
    Agent,
    Tmux,
    Human,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OutputFormat {
    Table,
    Json,
}

#[derive(Debug, Serialize)]
struct ActivityDocument {
    generated_at: String,
    range: String,
    since_at: Option<String>,
    until_at: Option<String>,
    rows: Vec<ActivityRow>,
}

#[derive(Debug, Serialize)]
struct ActivityRow {
    at: String,
    entry_type: &'static str,
    scope: String,
    started_at: Option<String>,
    ended_at: String,
    duration_secs: u64,
    duration: String,
    detail: String,
}

pub async fn run(cfg: &Config, args: Args) -> Result<()> {
    if !cfg.activity.enabled {
        bail!("activity ledger is disabled by config");
    }
    let Some(path) = cfg
        .activity
        .path
        .clone()
        .or_else(muxa::paths::default_activity_file)
    else {
        bail!("activity ledger path could not be resolved");
    };

    let range = parse_since(&args.since, OffsetDateTime::now_utc())?;
    let mut entries = muxa::activity::load(&path)
        .await
        .with_context(|| format!("loading activity ledger {}", path.display()))?;
    entries.retain(|entry| range.includes(entry.at()));
    if let Some(entry_type) = args.entry_type {
        entries.retain(|entry| entry_matches_type(entry, entry_type));
    }
    entries.sort_by_key(ActivityEntry::at);
    entries.reverse();
    if args.limit > 0 {
        entries.truncate(args.limit);
    }

    let doc = ActivityDocument {
        generated_at: format_rfc3339(OffsetDateTime::now_utc()),
        range: range.label,
        since_at: range.since_at.map(format_rfc3339),
        until_at: range.until_at.map(format_rfc3339),
        rows: entries.iter().map(row_for_entry).collect(),
    };

    match args.format {
        OutputFormat::Table => render_table(&doc, theme::for_config(cfg, args.theme, use_colors())),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&doc)?),
    }
    Ok(())
}

fn entry_matches_type(entry: &ActivityEntry, entry_type: EntryType) -> bool {
    matches!(
        (entry, entry_type),
        (ActivityEntry::StateTransition(_), EntryType::Agent)
            | (ActivityEntry::SessionForeground(_), EntryType::Tmux)
            | (ActivityEntry::HumanInteraction(_), EntryType::Human)
    )
}

fn row_for_entry(entry: &ActivityEntry) -> ActivityRow {
    match entry {
        ActivityEntry::StateTransition(entry) => {
            let started_at = entry.state_entered_at.or_else(|| {
                i64::try_from(entry.duration_secs)
                    .ok()
                    .map(|secs| entry.at - time::Duration::seconds(secs))
            });
            ActivityRow {
                at: format_rfc3339(entry.at),
                entry_type: "agent",
                scope: entry
                    .session_name
                    .clone()
                    .or_else(|| entry.pane.clone())
                    .unwrap_or_else(|| entry.session_id.clone()),
                started_at: started_at.map(format_rfc3339),
                ended_at: format_rfc3339(entry.at),
                duration_secs: entry.duration_secs,
                duration: format_duration(entry.duration_secs),
                detail: format!("{} {} -> {}", entry.kind, entry.from, entry.to),
            }
        }
        ActivityEntry::SessionForeground(entry) => ActivityRow {
            at: format_rfc3339(entry.ended_at),
            entry_type: "tmux",
            scope: entry.session_name.clone(),
            started_at: Some(format_rfc3339(entry.started_at)),
            ended_at: format_rfc3339(entry.ended_at),
            duration_secs: entry.duration_secs,
            duration: format_duration(entry.duration_secs),
            detail: "foreground attached".to_string(),
        },
        ActivityEntry::HumanInteraction(entry) => ActivityRow {
            at: format_rfc3339(entry.ended_at),
            entry_type: "human",
            scope: entry
                .session_name
                .clone()
                .or_else(|| entry.pane.clone())
                .or_else(|| entry.session_id.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            started_at: Some(format_rfc3339(entry.started_at)),
            ended_at: format_rfc3339(entry.ended_at),
            duration_secs: entry.duration_secs,
            duration: format_duration(entry.duration_secs),
            detail: human_kind_label(entry.kind).to_string(),
        },
    }
}

fn render_table(doc: &ActivityDocument, theme: CliTheme) {
    println!("muxa activity");
    println!("Range: {}", doc.range);
    if let Some(since_at) = doc.since_at.as_deref() {
        println!("Since: {since_at}");
    }
    if let Some(until_at) = doc.until_at.as_deref() {
        println!("Until: {until_at}");
    }
    println!("Rows: {}", doc.rows.len());
    println!();

    if doc.rows.is_empty() {
        println!("no activity ledger entries in this view");
        return;
    }

    let terminal_width = terminal_width();
    println!("{}", render_activity_table(doc, terminal_width, theme));
}

fn render_activity_table(doc: &ActivityDocument, terminal_width: usize, theme: CliTheme) -> String {
    let scope_width = if terminal_width >= 100 { 24 } else { 16 };
    let detail_width = terminal_width
        .saturating_sub(scope_width + 55)
        .clamp(14, 48);
    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Fixed(20)),
            ColumnConstraint::Absolute(Width::Fixed(7)),
            ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(scope_width).unwrap_or(u16::MAX),
            )),
            ColumnConstraint::Absolute(Width::Fixed(8)),
            ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(detail_width).unwrap_or(u16::MAX),
            )),
        ])
        .set_header(vec![
            theme.cell("AT", TableTone::Header),
            theme.cell("TYPE", TableTone::Header),
            theme.cell("SCOPE", TableTone::Header),
            theme.right_cell("DUR", TableTone::Header),
            theme.cell("DETAIL", TableTone::Header),
        ]);

    for row in &doc.rows {
        table.add_row(vec![
            theme.cell(truncate_cell(&row.ended_at, 20), TableTone::Dim),
            theme.cell(row.entry_type, activity_type_tone(row.entry_type)),
            theme.cell(truncate_cell(&row.scope, scope_width), TableTone::Accent),
            theme.right_cell(truncate_cell(&row.duration, 8), TableTone::Good),
            theme.cell(truncate_cell(&row.detail, detail_width), TableTone::Dim),
        ]);
    }
    format!("{table}")
}

fn activity_type_tone(entry_type: &str) -> TableTone {
    match entry_type {
        "agent" => TableTone::Warn,
        "tmux" => TableTone::Tmux,
        "human" => TableTone::Human,
        _ => TableTone::Dim,
    }
}

fn parse_since(raw: &str, now: OffsetDateTime) -> Result<TimeRange> {
    crate::time_range::parse_since(raw, now, "all retained activity")
}

fn human_kind_label(kind: HumanInteractionKind) -> &'static str {
    match kind {
        HumanInteractionKind::MuxaWatch => "muxa_watch",
        HumanInteractionKind::MuxaPromptInput => "muxa_prompt_input",
        HumanInteractionKind::TmuxAttach => "tmux_attach",
    }
}

fn format_rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
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
    use muxa::{
        HumanInteractionEntry, HumanInteractionInput, SessionForegroundEntry, StateTransitionEntry,
        StateTransitionInput,
    };
    use time::macros::datetime;

    #[test]
    fn parse_since_accepts_duration() {
        let range = parse_since("2h", datetime!(2026-06-01 12:00:00 UTC)).unwrap();
        assert_eq!(range.label, "last 2h");
        assert_eq!(range.since_at, Some(datetime!(2026-06-01 10:00:00 UTC)));
    }

    #[test]
    fn filters_by_entry_type() {
        let state =
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-01 12:00:00 UTC),
                kind: muxa::AgentKind::Codex,
                session_id: "agent".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: None,
                from: muxa::AgentState::Working,
                to: muxa::AgentState::Idle,
                state_entered_at: Some(datetime!(2026-06-01 11:59:00 UTC)),
            }));
        let tmux = ActivityEntry::SessionForeground(SessionForegroundEntry::new(
            "$1",
            "main",
            datetime!(2026-06-01 11:00:00 UTC),
            datetime!(2026-06-01 12:00:00 UTC),
        ));
        let human =
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaPromptInput,
                pane: Some("%1".into()),
                session_id: Some("$1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-01 11:50:00 UTC),
                ended_at: datetime!(2026-06-01 11:55:00 UTC),
            }));

        assert!(entry_matches_type(&state, EntryType::Agent));
        assert!(entry_matches_type(&tmux, EntryType::Tmux));
        assert!(entry_matches_type(&human, EntryType::Human));
        assert!(!entry_matches_type(&human, EntryType::Agent));
    }

    #[test]
    fn human_row_names_interaction_kind() {
        let entry =
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxAttach,
                pane: Some("%1".into()),
                session_id: Some("$1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-01 11:00:00 UTC),
                ended_at: datetime!(2026-06-01 11:01:30 UTC),
            }));

        let row = row_for_entry(&entry);

        assert_eq!(row.entry_type, "human");
        assert_eq!(row.scope, "main");
        assert_eq!(row.detail, "tmux_attach");
        assert_eq!(row.duration_secs, 90);
    }
}
