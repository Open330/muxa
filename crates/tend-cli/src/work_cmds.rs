//! `tend work done|list|show`: the CLI views over the Work model in
//! `tend_work::tmux_work`.

use crate::theme::TableTone;
use crate::tmux_work::{
    agent_summary, find_work_in, find_workspace, list_works, pane_pipeline, stage_for,
    stage_for_identity, stage_suffix, work_annotations, WorkInfo, WorkShowArgs,
};
use anyhow::{bail, Context, Result};
use std::path::Path;
use tend::work::{WorkRecord, WorkStage};

#[derive(Debug, clap::Args)]
pub struct WorkDoneArgs {
    /// Pipeline alias reporting done. Defaults to the alias recorded on
    /// the calling pane, so an agent can just run `tend work done`.
    #[arg(long)]
    pub alias: Option<String>,
    /// tmux pane to read the alias and work from. Defaults to `TMUX_PANE`.
    #[arg(long)]
    pub pane: Option<String>,
    /// Run generation being completed. Defaults to the generation stamped on
    /// the calling pane. Useful when automation supplies an explicit pane.
    #[arg(long)]
    pub generation: Option<u64>,
    /// Restart this alias's completion generation and hold/reconcile its
    /// downstream aliases again.
    #[arg(long, conflicts_with = "summary")]
    pub undo: bool,
    /// File a result record for the person with this completion.
    #[command(flatten)]
    pub record: crate::review_cmd::ResultRecordArgs,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkListArgs {
    /// Limit work windows to one managed workspace.
    #[arg(long)]
    pub workspace: Option<String>,
    /// List Work records at this stage instead of live windows: `queued` is
    /// the backlog of planned Work; also `in_progress`, `review`, `done`, `auto`.
    #[arg(long, value_parser = crate::work_backlog::parse_stage)]
    pub stage: Option<tend::work::WorkStage>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

/// Report that this agent has finished its part, which is what opens an
/// `after` edge for whatever waits on it.
///
/// tend cannot observe "done" any other way: agent state says `idle` both
/// when an agent has finished and when it is between thoughts, and a pane
/// stays open either way. So the agent says so, and tend records the claim
/// rather than inferring one.
pub async fn run_work_done(
    args: WorkDoneArgs,
    client: &tend::ipc::Client,
    config: &tend::config::Config,
) -> Result<()> {
    let pane = args
        .pane
        .clone()
        .or_else(crate::mcp::own_tmux_pane)
        .filter(|pane| !pane.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("no pane to read; pass --pane (outside tmux, or under a shared Codex app-server whose TMUX_PANE names another pane)"))?;
    let (identity, alias, generation) = pane_pipeline(&pane, args.alias.clone(), args.generation)?;
    let record = if args.undo {
        None
    } else {
        args.record.into_review(&identity, &alias, generation)?
    };
    let mut filed = None;
    let run = if args.undo {
        client
            .pipeline_invalidate(&identity, &alias, generation)
            .await
            .with_context(|| format!("invalidate pipeline alias {alias:?}"))?
    } else if let Some(record) = record {
        crate::review_cmd::require_filing(client, &record).await?;
        if crate::decision_record::daemon_supports(client, tend::work_complete::CAPABILITY)
            .await
            .map_err(anyhow::Error::msg)?
        {
            // One daemon operation: the record and the completion land
            // together, or a crash between them is finished at restart.
            let (run, item) = client
                .pipeline_done_with_record(&identity, &alias, generation, &record)
                .await
                .with_context(|| format!("file the result record and complete {alias:?}"))?;
            filed = Some(item);
            run
        } else {
            // An older daemon: filed before completion, so a refused record
            // leaves the alias open and a retry (same key, same item)
            // files and completes together.
            filed = Some(
                crate::review_cmd::file(client, &record)
                    .await
                    .context("filing the result record; the alias is not reported done yet")?,
            );
            client
                .pipeline_done(&identity, &alias, generation)
                .await
                .with_context(|| format!("atomically complete pipeline alias {alias:?}"))?
        }
    } else {
        client
            .pipeline_done(&identity, &alias, generation)
            .await
            .with_context(|| format!("atomically complete pipeline alias {alias:?}"))?
    };
    // Completion is the event that drives reconciliation; callers never need
    // to remember a second `work up`. The daemon also subscribes to this same
    // durable revision as a crash/restart safety net.
    crate::work_up::reconcile_run(client, config, &identity, run.generation).await?;
    let done = run
        .aliases
        .values()
        .filter(|state| state.status == tend::pipeline_run::PipelineAliasStatus::Done)
        .map(|state| state.alias.clone())
        .collect::<Vec<_>>();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "work": identity,
                "alias": alias,
                "generation": run.generation,
                "done": done,
                "record": filed.as_ref().map(|item| &item.id),
            }))?
        );
    } else if args.undo {
        println!("{alias} is no longer reported done");
    } else {
        println!("{alias} reported done ({})", done.join(", "));
        if let Some(item) = &filed {
            println!(
                "result record {} (generation {}): tend review show {}",
                item.id, item.generation, item.id
            );
        }
    }
    Ok(())
}

pub async fn run_work_list(args: WorkListArgs, client: &tend::ipc::Client) -> Result<()> {
    let works = match args.workspace.as_deref() {
        Some(workspace) => find_workspace(workspace)?
            .map(|workspace| workspace.works)
            .unwrap_or_default(),
        None => list_works()?,
    };
    let visible_runs = client
        .pipeline_runs()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|run| {
            args.workspace
                .as_deref()
                .is_none_or(|workspace| run.identity.workspace_id == workspace)
        })
        .collect::<Vec<_>>();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "works": works,
                "pipeline_runs": visible_runs,
            }))?
        );
    } else if works.is_empty() && visible_runs.is_empty() {
        println!("no tend-managed work or durable pipeline Runs");
    } else {
        let theme = crate::theme::for_theme(
            tend::config::Config::load_or_default(None)
                .map(|cfg| cfg.ui.theme)
                .unwrap_or_default(),
            crate::use_colors(),
        );
        print!(
            "{}",
            work_list_table(&works, &work_annotations(), &visible_runs, theme)
        );
    }
    Ok(())
}

/// Render the work table.
///
/// Pure on its inputs so the layout is testable without a tmux server; the
/// caller supplies the works, their annotations, and the theme.
fn work_list_table(
    works: &[WorkInfo],
    records: &[WorkRecord],
    runs: &[tend::pipeline_run::PipelineRun],
    theme: crate::theme::CliTheme,
) -> String {
    use comfy_table::{ContentArrangement, Table};

    let staged: Vec<Option<String>> = works
        .iter()
        .map(|work| {
            stage_for(work, records)
                .filter(|stage| *stage != WorkStage::Auto)
                .map(|stage| stage.label().to_string())
        })
        .collect();
    let run_stages = runs
        .iter()
        .map(|run| {
            stage_for_identity(&run.identity, records)
                .filter(|stage| *stage != WorkStage::Auto)
                .map(|stage| stage.label().to_string())
        })
        .collect::<Vec<_>>();
    // The column only earns its width when something is actually staged.
    let show_stage = staged.iter().chain(&run_stages).any(Option::is_some);

    let mut header = vec!["WORK", "WORKSPACE", "GEN", "ALIASES", "DONE"];
    if show_stage {
        header.push("STAGE");
    }
    header.push("CWD");

    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(
            header
                .into_iter()
                .map(|label| theme.cell(label, TableTone::Header))
                .collect::<Vec<_>>(),
        );

    for (work, stage) in works.iter().zip(&staged) {
        let run = runs.iter().find(|run| {
            run.identity.workspace_id == work.workspace && run.identity.work_id == work.work
        });
        let completion = run.map(tend::pipeline_run::PipelineRun::completion);
        let mut row = vec![
            theme.cell(&work.work, TableTone::Accent),
            theme.cell(&work.workspace, TableTone::Dim),
            run.map_or_else(
                || theme.right_cell("-", TableTone::Dim),
                |run| theme.right_cell(run.generation, TableTone::Dim),
            ),
            theme.cell(
                run.map_or_else(|| agent_summary(work), pipeline_alias_summary),
                TableTone::Tmux,
            ),
            match completion {
                // No Run, or a Run that named no agents: nothing here was ever
                // asked to report, and `0/N` would say the opposite.
                None | Some((_, 0)) => theme.right_cell("-", TableTone::Dim),
                Some((done, total)) if done == total => {
                    theme.right_cell(format!("{done}/{total}"), TableTone::Good)
                }
                Some((done, total)) => theme.right_cell(format!("{done}/{total}"), TableTone::Warn),
            },
        ];
        if show_stage {
            row.push(theme.cell(stage.as_deref().unwrap_or("-"), TableTone::Dim));
        }
        row.push(theme.cell(shorten_home(&work.cwd), TableTone::Dim));
        table.add_row(row);
    }
    for (run, stage) in runs.iter().zip(&run_stages).filter(|(run, _)| {
        !works.iter().any(|work| {
            run.identity.workspace_id == work.workspace && run.identity.work_id == work.work
        })
    }) {
        let (done, total) = run.completion();
        let mut row = vec![
            theme.cell(&run.identity.work_id, TableTone::Accent),
            theme.cell(&run.identity.workspace_id, TableTone::Dim),
            theme.right_cell(run.generation, TableTone::Dim),
            theme.cell(pipeline_alias_summary(run), TableTone::Tmux),
            if done == total {
                theme.right_cell(format!("{done}/{total}"), TableTone::Good)
            } else {
                theme.right_cell(format!("{done}/{total}"), TableTone::Warn)
            },
        ];
        if show_stage {
            row.push(theme.cell(stage.as_deref().unwrap_or("-"), TableTone::Dim));
        }
        row.push(theme.cell(shorten_home(&run.cwd), TableTone::Dim));
        table.add_row(row);
    }
    format!("{table}\n")
}

fn pipeline_alias_summary(run: &tend::pipeline_run::PipelineRun) -> String {
    run.desired
        .iter()
        .filter_map(|agent| {
            run.aliases
                .get(&agent.alias)
                .map(|state| format!("{}:{}", agent.alias, state.status))
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// `/home/june/x` -> `~/x`, so the column spends its width on the part that
/// differs between rows.
fn shorten_home(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    path.strip_prefix(&home).map_or_else(
        |_| path.display().to_string(),
        |rest| {
            if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            }
        },
    )
}

fn show_json(work: Option<&WorkInfo>, show: &tend::ipc::WorkShow) -> serde_json::Value {
    serde_json::json!({
        "window": work,
        "record": show.record,
        "pipeline_run": show.run,
        "review_items": show.reviews,
        "review_has_more": show.reviews_more,
        "review_error": show.reviews_error,
    })
}

/// One Work: its live window when there is one, then its durable record
/// (what was asked, done and decided), which outlives the window.
pub async fn run_work_show(args: WorkShowArgs, client: &tend::ipc::Client) -> Result<()> {
    let work = find_work_in(&args.work, args.workspace.as_deref())?;
    let supported = crate::work_backlog::backlog_supported(client).await;
    let identity = if let Some(work) = &work {
        Some(tend::work::WorkIdentity::new(&work.workspace, &work.work))
    } else if supported {
        // No window: resolve from the durable records, the way `work up`
        // starts a plan, not from whatever directory this runs in.
        let records = client
            .work_backlog(None)
            .await
            .context("reading the Work backlog")?;
        let here = std::env::current_dir()?;
        crate::work_backlog::closed_work_identity(
            &records,
            &args.work,
            args.workspace.as_deref(),
            &here,
        )?
    } else {
        None
    };
    let show = match identity {
        Some(identity) if supported => client.work_record_show(&identity).await.ok(),
        _ => None,
    };
    let Some(work) = work else {
        let Some(show) = show else {
            bail!("managed work {:?} not found", args.work);
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(&show_json(None, &show))?);
        } else {
            print!(
                "{}",
                crate::work_backlog::render_show(&show, time::OffsetDateTime::now_utc())
            );
        }
        return Ok(());
    };
    if let (true, Some(show)) = (args.json, show.as_ref()) {
        println!(
            "{}",
            serde_json::to_string_pretty(&show_json(Some(&work), show))?
        );
    } else if args.json {
        println!("{}", serde_json::to_string_pretty(&work)?);
    } else {
        let records = work_annotations();
        println!(
            "{}  workspace={}  session={}  window={}  cwd={}{}",
            work.work,
            work.workspace,
            work.session,
            work.window,
            work.cwd.display(),
            stage_suffix(&work, &records)
        );
        for agent in &work.agents {
            println!(
                "  {}  {}{}{}{}",
                agent.pane,
                agent.agent,
                agent
                    .alias
                    .as_deref()
                    .map_or_else(String::new, |alias| format!(" alias={alias}")),
                agent
                    .role
                    .as_deref()
                    .map_or_else(String::new, |role| format!(" role={role}")),
                agent
                    .task
                    .as_deref()
                    .map_or_else(String::new, |task| format!(" task={task}"))
            );
        }
    }
    if !args.json {
        if let Some(show) = &show {
            let record = crate::work_backlog::render_show(show, time::OffsetDateTime::now_utc());
            if !record.is_empty() {
                print!("\n{record}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod work_list_view_tests {
    use super::*;
    use crate::tmux_work::ManagedAgentPane;
    use std::path::PathBuf;

    fn agent(pane: &str, program: &str, alias: Option<&str>) -> ManagedAgentPane {
        ManagedAgentPane {
            pane: pane.into(),
            agent: program.into(),
            alias: alias.map(Into::into),
            role: None,
            task: None,
            command: program.into(),
            cwd: PathBuf::from("/repo"),
        }
    }

    fn run(work: &str, aliases: &[(&str, bool)]) -> tend::pipeline_run::PipelineRun {
        use tend::pipeline_run::{PipelineAliasState, PipelineAliasStatus};
        tend::pipeline_run::PipelineRun {
            identity: tend::work::WorkIdentity::new("webapp", work),
            pipeline: "pair".into(),
            desired: aliases
                .iter()
                .map(|(alias, _)| tend::pipeline::DesiredAgent {
                    alias: (*alias).to_string(),
                    program: "claude".into(),
                    role: None,
                    task: None,
                    prompt: None,
                    options: Vec::new(),
                    direction: None,
                    after: Vec::new(),
                    purpose: tend::collaboration::LaunchPurpose::Primary,
                })
                .collect(),
            cwd: PathBuf::from("/repo"),
            generation: 1,
            window_id: None,
            aliases: aliases
                .iter()
                .map(|(alias, done)| {
                    (
                        (*alias).to_string(),
                        PipelineAliasState {
                            alias: (*alias).to_string(),
                            status: if *done {
                                PipelineAliasStatus::Done
                            } else {
                                PipelineAliasStatus::Pending
                            },
                            generation: 1,
                            completion_generation: None,
                            pane: None,
                            error: None,
                            reconcile_pending: false,
                            claim_started_at: None,
                            completing: None,
                            updated_at: time::OffsetDateTime::UNIX_EPOCH,
                        },
                    )
                })
                .collect(),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn work(name: &str, agents: Vec<ManagedAgentPane>) -> WorkInfo {
        WorkInfo {
            work: name.into(),
            workspace: "webapp".into(),
            session: "webapp".into(),
            session_id: "$1".into(),
            window: "@1".into(),
            window_index: 0,
            window_name: name.into(),
            cwd: PathBuf::from("/repo"),
            external_item: None,
            agents,
        }
    }

    fn durable_run() -> tend::pipeline_run::PipelineRun {
        use tend::pipeline_run::{PipelineAliasState, PipelineAliasStatus, PipelineRun};
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let desired = [("impl", Vec::new()), ("review", vec!["impl".to_string()])]
            .into_iter()
            .map(|(alias, after)| tend::pipeline::DesiredAgent {
                alias: alias.to_string(),
                program: "codex".to_string(),
                role: None,
                task: None,
                prompt: None,
                options: Vec::new(),
                direction: None,
                after,
                purpose: tend::collaboration::LaunchPurpose::Primary,
            })
            .collect();
        let state = |alias: &str, status, completion_generation| PipelineAliasState {
            alias: alias.to_string(),
            status,
            generation: 1,
            completion_generation,
            pane: (alias == "impl").then(|| "%1".to_string()),
            error: None,
            reconcile_pending: false,
            claim_started_at: None,
            completing: None,
            updated_at: now,
        };
        PipelineRun {
            identity: tend::work::WorkIdentity::new("webapp", "ENG-1"),
            pipeline: "delivery".to_string(),
            desired,
            cwd: PathBuf::from("/repo"),
            generation: 1,
            window_id: Some("@1".to_string()),
            aliases: std::collections::BTreeMap::from([
                (
                    "impl".to_string(),
                    state("impl", PipelineAliasStatus::Done, Some(1)),
                ),
                (
                    "review".to_string(),
                    state("review", PipelineAliasStatus::Pending, None),
                ),
            ]),
            updated_at: now,
        }
    }

    /// `done` records aliases, so only aliased panes can ever be in it. A pane
    /// the operator split by hand must not inflate the denominator and make a
    #[test]
    fn agents_read_as_aliases_or_collapsed_programs() {
        let piped = work(
            "ENG-1",
            vec![
                agent("%1", "claude", Some("impl")),
                agent("%2", "codex", Some("review")),
            ],
        );
        assert_eq!(agent_summary(&piped), "impl \u{b7} review");

        let manual = work(
            "RELEASE-1",
            vec![
                agent("%1", "claude", None),
                agent("%2", "claude", None),
                agent("%3", "codex", None),
            ],
        );
        assert_eq!(agent_summary(&manual), "claude x2 \u{b7} codex");
    }

    /// A converged pipeline and a stalled one differ by one cell, so that cell
    /// has to be present and correct.
    #[test]
    fn table_shows_the_completion_ratio() {
        let works = vec![
            work(
                "ENG-1",
                vec![
                    agent("%1", "claude", Some("impl")),
                    agent("%2", "codex", Some("review")),
                ],
            ),
            work("RELEASE-1", vec![agent("%3", "claude", None)]),
        ];
        let runs = vec![run("ENG-1", &[("impl", true), ("review", true)])];
        let out = work_list_table(&works, &[], &runs, crate::theme::CliTheme::plain());
        assert!(out.contains("ENG-1"), "{out}");
        assert!(out.contains("2/2"), "{out}");
        // The stage column stays out of the way when nothing is staged.
        assert!(!out.contains("STAGE"), "{out}");
    }

    /// The denominator is what the pipeline *asked for*, not what has been
    /// launched — otherwise a pair whose reviewer has not started yet reads
    /// `1/1`, which is indistinguishable from converged.
    #[test]
    fn an_unlaunched_agent_still_counts_against_the_total() {
        let works = vec![work("ENG-1", vec![agent("%1", "claude", Some("impl"))])];
        let runs = vec![run("ENG-1", &[("impl", true), ("review", false)])];
        let out = work_list_table(&works, &[], &runs, crate::theme::CliTheme::plain());
        assert!(out.contains("1/2"), "{out}");
    }

    /// A window with no Run has no completion record at all. `0/N` would say
    /// nothing finished; the truth is that nothing here was ever asked to.
    #[test]
    fn a_work_without_a_run_reports_no_ratio() {
        let works = vec![work("ENG-1", vec![agent("%1", "claude", Some("impl"))])];
        let out = work_list_table(&works, &[], &[], crate::theme::CliTheme::plain());
        assert!(!out.contains("0/1"), "{out}");
        assert!(out.contains("ENG-1"), "{out}");
    }

    #[test]
    fn table_uses_durable_desired_aliases_before_downstream_has_a_pane() {
        let works = vec![work("ENG-1", vec![agent("%1", "codex", Some("impl"))])];
        let out = work_list_table(
            &works,
            &[],
            &[durable_run()],
            crate::theme::CliTheme::plain(),
        );
        assert!(out.contains("1/2"), "{out}");
        assert!(out.contains("impl:done"), "{out}");
        assert!(out.contains("review:pending"), "{out}");
        assert!(!out.contains("1/1"), "{out}");
    }

    #[test]
    fn table_keeps_a_durable_run_visible_without_a_tmux_window() {
        let out = work_list_table(&[], &[], &[durable_run()], crate::theme::CliTheme::plain());
        assert!(out.contains("ENG-1"), "{out}");
        assert!(out.contains("webapp"), "{out}");
        assert!(out.contains("impl:done"), "{out}");
        assert!(out.contains("review:pending"), "{out}");
        assert!(out.contains("1/2"), "{out}");
    }
}
