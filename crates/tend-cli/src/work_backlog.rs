//! The Work backlog: things the person wants done, planned without
//! launching anything, started later with `tend work up`, and kept with
//! what was asked, done and decided after their window closes.

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use std::path::Path;
use tend::ipc::{Client, WorkShow};
use tend::work::{WorkEdit, WorkIdentity, WorkRecord, WorkStage, WORK_BACKLOG_CAPABILITY};
use time::OffsetDateTime;

#[derive(Debug, clap::Args)]
pub struct PlanArgs {
    /// Work id, or WORKSPACE/WORK.
    pub work: String,
    /// Workspace id. Defaults to the WORKSPACE/ prefix, then the cwd's name.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Short name shown in lists.
    #[arg(long)]
    pub title: Option<String>,
    /// What done looks like.
    #[arg(long)]
    pub goal: Option<String>,
    /// The next concrete step.
    #[arg(long = "next")]
    pub next_action: Option<String>,
    /// `queued` (default for a new plan), `in_progress`, `review`, `done` or `auto`.
    #[arg(long, value_parser = parse_stage)]
    pub stage: Option<WorkStage>,
    /// Directory to start in. A new plan defaults to the current one.
    #[arg(long)]
    pub cwd: Option<std::path::PathBuf>,
    /// `[pipeline.<name>]` to start with.
    #[arg(long)]
    pub pipeline: Option<String>,
    /// The instruction the agents start from (`work up --body`).
    #[arg(long, visible_alias = "body")]
    pub prompt: Option<String>,
    /// External issue key to link when it starts (ENG-123, #79).
    #[arg(long)]
    pub external: Option<String>,
    /// Backlog order: lower first.
    #[arg(long, allow_negative_numbers = true, conflicts_with = "no_priority")]
    pub priority: Option<i32>,
    /// Remove the priority.
    #[arg(long)]
    pub no_priority: bool,
    #[arg(long)]
    pub json: bool,
}

pub(crate) fn parse_stage(raw: &str) -> Result<WorkStage, String> {
    serde_json::from_value(serde_json::Value::String(raw.trim().replace('-', "_")))
        .map_err(|_| format!("unknown stage {raw:?}: auto, queued, in_progress, review or done"))
}

async fn require_backlog(client: &Client) -> Result<()> {
    let hello = client
        .hello(std::time::Duration::from_secs(3))
        .await
        .context("asking tendd what it supports")?;
    if !hello
        .capabilities
        .iter()
        .any(|c| c == WORK_BACKLOG_CAPABILITY)
    {
        bail!("this tendd does not serve the Work backlog ({WORK_BACKLOG_CAPABILITY}); restart it with the same tend build");
    }
    Ok(())
}

/// Whether the daemon serves the backlog; used where it is optional.
pub(crate) async fn backlog_supported(client: &Client) -> bool {
    client
        .hello(std::time::Duration::from_secs(2))
        .await
        .is_ok_and(|hello| {
            hello
                .capabilities
                .iter()
                .any(|c| c == WORK_BACKLOG_CAPABILITY)
        })
}

/// Resolve `WORK` / `WORKSPACE/WORK` plus `--workspace` to an identity,
/// with the same normalization `tend work up` applies.
pub(crate) fn identity(work: &str, workspace: Option<&str>, cwd: &Path) -> Result<WorkIdentity> {
    let (prefix, work) = match work.split_once('/') {
        Some((workspace, work)) if workspace.trim().is_empty() => {
            bail!("empty workspace in {workspace:?}/{work:?}")
        }
        Some((prefix, work)) => (Some(prefix), work),
        None => (None, work),
    };
    let workspace = match (workspace, prefix) {
        (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b) => {
            bail!("--workspace {a:?} disagrees with {b:?}/")
        }
        (Some(workspace), _) | (None, Some(workspace)) => {
            crate::tmux_work::normalize_workspace_id(workspace)?
        }
        (None, None) => crate::tmux_work::workspace_id_for_cwd(cwd)?,
    };
    Ok(WorkIdentity::new(
        workspace,
        crate::tmux_work::normalize_work_id(work)?,
    ))
}

pub async fn run_plan(args: PlanArgs, client: &Client) -> Result<()> {
    require_backlog(client).await?;
    let here = std::env::current_dir().context("reading the current directory")?;
    let identity = identity(&args.work, args.workspace.as_deref(), &here)?;
    let exists = client
        .work_backlog(None)
        .await?
        .iter()
        .any(|record| record.identity == identity);
    let cwd = match args.cwd {
        Some(cwd) => Some(std::path::absolute(&cwd)?),
        None if !exists => Some(here),
        None => None,
    };
    let edit = WorkEdit {
        title: args.title,
        goal: args.goal,
        next_action: args.next_action,
        stage: args.stage,
        cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
        pipeline: args.pipeline,
        prompt: args.prompt,
        external: args.external,
        priority: args.priority,
        clear_priority: args.no_priority,
    };
    let record = client.work_edit(&identity, &edit).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        println!(
            "{} {}",
            if exists { "updated" } else { "planned" },
            backlog_line(&record)
        );
        if !exists {
            println!(
                "start it with: tend work up {} --workspace {}",
                record.identity.work_id, record.identity.workspace_id
            );
        }
    }
    Ok(())
}

/// `tend work list --stage STAGE`: the backlog rather than live windows.
pub async fn run_list_stage(
    client: &Client,
    stage: WorkStage,
    workspace: Option<&str>,
    json: bool,
) -> Result<()> {
    require_backlog(client).await?;
    let mut records = client.work_backlog(Some(stage)).await?;
    if let Some(workspace) = workspace {
        records.retain(|record| record.identity.workspace_id.eq_ignore_ascii_case(workspace));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else if records.is_empty() {
        println!("no Work at stage {}", stage.label());
    } else {
        for record in &records {
            println!("{}", backlog_line(record));
        }
    }
    Ok(())
}

fn backlog_line(record: &WorkRecord) -> String {
    let plan = record.plan.clone().unwrap_or_default();
    let mut line = format!(
        "{}  {:<11}",
        record.identity.key(),
        record.metadata.stage.label()
    );
    if let Some(priority) = plan.priority {
        let _ = write!(line, " p{priority}");
    }
    let title = record
        .metadata
        .title
        .as_deref()
        .or(record.metadata.goal.as_deref())
        .or(plan.prompt.as_deref())
        .unwrap_or("");
    let _ = write!(line, "  {}", first_line(title));
    line
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// The record section of `tend work show`: what was asked, what was done
/// (the durable Run), and what was decided (result records and their
/// dispositions).
pub(crate) fn render_show(show: &WorkShow, now: OffsetDateTime) -> String {
    let mut out = String::new();
    if let Some(record) = &show.record {
        let _ = writeln!(out, "record:     {}", record.identity.key());
        let _ = writeln!(out, "stage:      {}", record.metadata.stage.label());
        if let Some(title) = &record.metadata.title {
            let _ = writeln!(out, "title:      {title}");
        }
        if let Some(goal) = &record.metadata.goal {
            let _ = writeln!(out, "goal:       {}", goal.replace('\n', "\n            "));
        }
        if let Some(next) = &record.metadata.next_action {
            let _ = writeln!(out, "next:       {next}");
        }
        if let Some(plan) = &record.plan {
            if let Some(prompt) = &plan.prompt {
                let _ = writeln!(
                    out,
                    "asked:      {}",
                    prompt.replace('\n', "\n            ")
                );
            }
            for (label, value) in [
                ("cwd", plan.cwd.as_deref()),
                ("pipeline", plan.pipeline.as_deref()),
                ("external", plan.external.as_deref()),
            ] {
                if let Some(value) = value {
                    let _ = writeln!(out, "{label:<11} {value}");
                }
            }
            if let Some(priority) = plan.priority {
                let _ = writeln!(out, "priority:   {priority}");
            }
        }
        for item in &record.external_items {
            let _ = writeln!(
                out,
                "issue:      {} {}{}",
                item.display_key,
                item.title.as_deref().unwrap_or(""),
                item.status
                    .as_deref()
                    .map_or_else(String::new, |status| format!(" [{status}]"))
            );
        }
    }
    if let Some(run) = &show.run {
        let _ = writeln!(out, "done:       run generation {}", run.generation);
        for state in run.aliases.values() {
            let _ = writeln!(out, "  {:<12} {:?}", state.alias, state.status);
        }
    }
    if !show.reviews.is_empty() {
        let _ = writeln!(out, "decided:");
        for item in &show.reviews {
            let state = item.disposition.as_ref().map_or_else(
                || "open".to_owned(),
                |d| {
                    let note = d
                        .note
                        .as_deref()
                        .map_or_else(String::new, |note| format!(" — {}", first_line(note)));
                    format!("{:?}{note}", d.kind).to_lowercase()
                },
            );
            let alias = match &item.target {
                tend::review::ReviewTarget::Work {
                    alias: Some(alias), ..
                } => format!("@{alias} "),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "  {}  {:>4}  {alias}{}  [{state}]",
                item.id,
                crate::attend::humanize_since(item.requested_at, now),
                first_line(&item.summary),
            );
        }
        if show.reviews_more {
            out.push_str("  … more: tend review list --all\n");
        }
    }
    if let Some(error) = &show.reviews_error {
        let _ = writeln!(out, "unavailable: {error}");
    }
    out
}

/// Fill `tend work up` arguments the caller left out from the Work's plan,
/// if one exists. Explicit arguments always win.
pub(crate) fn fill_from_plan(args: &mut crate::work_up::UpArgs, record: &WorkRecord) {
    let Some(plan) = &record.plan else {
        return;
    };
    if args.workspace.is_none() {
        args.workspace = Some(record.identity.workspace_id.clone());
    }
    if args.cwd.is_none() {
        args.planned_cwd = plan.cwd.clone().map(Into::into);
    }
    if args.pipeline.is_none() {
        args.pipeline.clone_from(&plan.pipeline);
    }
    if args.body.is_none() {
        args.body.clone_from(&plan.prompt);
    }
    if args.external.is_none() {
        args.external.clone_from(&plan.external);
    }
}

/// The planned record `tend work up WORK` should start from: the one with
/// this Work id in the named workspace, or the only one when no workspace
/// was named.
pub(crate) fn planned_record<'a>(
    records: &'a [WorkRecord],
    work: &str,
    workspace: Option<&str>,
) -> Option<&'a WorkRecord> {
    let work = crate::tmux_work::normalize_work_id(work).ok()?;
    let workspace = workspace.and_then(|w| crate::tmux_work::normalize_workspace_id(w).ok());
    let mut matching = records.iter().filter(|record| {
        record.plan.is_some()
            && record.identity.work_id == work
            && workspace
                .as_ref()
                .is_none_or(|w| &record.identity.workspace_id == w)
    });
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tend::work::WorkPlan;
    use time::macros::datetime;

    fn record(workspace: &str, work: &str, plan: Option<WorkPlan>) -> WorkRecord {
        WorkRecord {
            identity: WorkIdentity::new(workspace, work),
            metadata: tend::work::WorkMetadata {
                title: Some("Fix login".into()),
                goal: None,
                next_action: None,
                stage: WorkStage::Queued,
                updated_at: datetime!(2026-11-12 10:00 UTC),
            },
            external_items: Vec::new(),
            legacy_binding: None,
            plan,
            created_at: datetime!(2026-11-12 10:00 UTC),
        }
    }

    #[test]
    fn identity_accepts_both_forms_and_normalizes() {
        let cwd = Path::new("/src/Tend");
        assert_eq!(
            identity("auth-cleanup", None, cwd).unwrap(),
            WorkIdentity::new("tend", "AUTH-CLEANUP")
        );
        assert_eq!(
            identity("Web/eng-1", None, cwd).unwrap(),
            WorkIdentity::new("web", "ENG-1")
        );
        assert!(identity("web/eng-1", Some("api"), cwd).is_err());
        assert_eq!(parse_stage("in-progress").unwrap(), WorkStage::InProgress);
        assert!(parse_stage("later").is_err());
    }

    #[test]
    fn work_up_fills_only_what_was_not_given() {
        let plan = WorkPlan {
            cwd: Some("/src/web".into()),
            pipeline: Some("pair".into()),
            prompt: Some("fix the login redirect".into()),
            external: None,
            priority: Some(1),
        };
        let records = vec![
            record("web", "ENG-1", Some(plan)),
            record("api", "ENG-2", None),
        ];
        let planned = planned_record(&records, "eng-1", None).unwrap();
        let mut args: crate::work_up::UpArgs =
            clap::Parser::try_parse_from(["up", "ENG-1", "--pipeline", "solo"])
                .map(|wrapper: UpWrapper| wrapper.args)
                .unwrap();
        fill_from_plan(&mut args, planned);
        assert_eq!(args.workspace.as_deref(), Some("web"));
        assert!(args.cwd.is_none(), "a plan's cwd is not an explicit --cwd");
        assert_eq!(args.planned_cwd.as_deref(), Some(Path::new("/src/web")));
        assert_eq!(args.pipeline.as_deref(), Some("solo"), "explicit wins");
        assert_eq!(args.body.as_deref(), Some("fix the login redirect"));
        assert!(planned_record(&records, "ENG-2", None).is_none(), "no plan");
        assert!(planned_record(&records, "ENG-1", Some("api")).is_none());
    }

    #[derive(clap::Parser)]
    struct UpWrapper {
        #[command(flatten)]
        args: crate::work_up::UpArgs,
    }

    #[test]
    fn show_says_what_was_asked_done_and_decided() {
        let mut planned = record(
            "web",
            "ENG-1",
            Some(WorkPlan {
                prompt: Some("fix the redirect".into()),
                ..WorkPlan::default()
            }),
        );
        planned.metadata.stage = WorkStage::Review;
        let review: tend::review::ReviewItem = serde_json::from_value(serde_json::json!({
            "id": "rv_1", "generation": 1, "summary": "Redirect fixed",
            "target": {"kind": "work", "workspace": "web", "work": "ENG-1", "alias": "impl"},
            "requested_at": "2026-11-12T11:00:00Z", "requested_by": {"kind": "agent"},
            "disposition": {"kind": "accepted", "by": {"kind": "user"},
                            "at": "2026-11-12T11:30:00Z", "note": "ship it"}
        }))
        .unwrap();
        let out = render_show(
            &WorkShow {
                record: Some(planned),
                run: None,
                reviews: vec![review],
                reviews_more: false,
                reviews_error: None,
            },
            datetime!(2026-11-12 12:00 UTC),
        );
        assert!(out.contains("stage:      review"), "{out}");
        assert!(out.contains("asked:      fix the redirect"), "{out}");
        assert!(
            out.contains("rv_1    1h  @impl Redirect fixed  [accepted — ship it]"),
            "{out}"
        );
    }
}
