//! `tend work up`: the CLI entry point, plan lookup and first-run onboarding
//! around `tend_work::work_up`.

use crate::work_up::{
    apply, apply_durable, print_prompts, print_result, resolve, Resolved, UpArgs,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tend::config::Config;
use tend::pipeline;

pub async fn run(
    mut args: UpArgs,
    config: &Config,
    config_path: Option<PathBuf>,
    client: Option<&tend::ipc::Client>,
) -> Result<()> {
    // A planned Work starts from its plan: arguments left out come from
    // the backlog record, and starting it moves it to in progress.
    let planned = match client {
        Some(client) if crate::work_backlog::backlog_supported(client).await => {
            let records = client
                .work_backlog(None)
                .await
                .context("reading the Work backlog")?;
            crate::work_backlog::find_record(&records, &args.work, args.workspace.as_deref(), true)?
                .cloned()
        }
        _ => None,
    };
    if let Some(record) = &planned {
        crate::work_backlog::fill_from_plan(&mut args, record);
    }
    let json = args.json;
    let dry_run = args.dry_run;
    let show_prompts = args.show_prompts;
    let resolved = resolve_or_onboard(&args, config, config_path, client).await?;
    if show_prompts {
        print_prompts(&resolved.desired);
    }
    let result = if dry_run {
        apply(resolved, true)?
    } else {
        let client = client.ok_or_else(|| {
            anyhow::anyhow!("tend work up requires tendd for durable pipeline state")
        })?;
        let result = apply_durable(resolved, client).await?;
        if let Some(record) = planned
            .as_ref()
            .filter(|record| record.metadata.stage == tend::work::WorkStage::Queued)
        {
            let started = tend::work::WorkEdit {
                stage: Some(tend::work::WorkStage::InProgress),
                ..tend::work::WorkEdit::default()
            };
            if let Err(error) = client.work_edit(&record.identity, &started).await {
                eprintln!("note: started, but the backlog record stays queued: {error}");
            }
        }
        result
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_result(&result);
    }
    Ok(())
}

/// Resolve, and on a first run with nothing configured, offer to set it
/// up rather than failing with instructions the operator then has to
/// follow by hand.
///
/// "No route matches" is only an error for someone who already has a
/// config. For everyone else it is the first thing tend ever says to
/// them, and answering it with TOML syntax is how a feature goes unused.
/// `tend work init` is already the conversation that fixes it, so this
/// hands over to it and retries once with the config it wrote.
async fn resolve_or_onboard(
    args: &UpArgs,
    config: &Config,
    config_path: Option<PathBuf>,
    client: Option<&tend::ipc::Client>,
) -> Result<Resolved> {
    let error = match resolve(args, config, client).await {
        Ok(resolved) => return Ok(resolved),
        Err(error) => error,
    };
    let unconfigured = error
        .downcast_ref::<pipeline::PipelineError>()
        .is_some_and(|error| matches!(error, pipeline::PipelineError::NoRoute(_)));
    if !unconfigured || !offer_onboarding()? {
        return Err(error);
    }
    crate::work_init::run(
        crate::work_init::InitArgs {
            describe: None,
            agent: None,
            dry_run: false,
            yes: false,
        },
        config,
        config_path.clone(),
    )
    .await?;
    // Reload: the file just changed underneath the config this process
    // started with.
    let config = Config::load_or_default(config_path.as_deref())
        .context("re-reading the config tend work init just wrote")?;
    resolve(args, &config, client).await
}

/// Ask whether to set tend up now. Non-interactive callers keep the plain
/// error — a script wants a failure it can read, not a prompt.
fn offer_onboarding() -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(false);
    }
    println!("tend has no work pipeline configured yet.");
    Ok(cliclack::confirm("Set one up now?")
        .initial_value(true)
        .interact()?)
}
