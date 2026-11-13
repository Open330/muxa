//! The planned-Work lookups `work up` needs: whether the daemon serves the
//! backlog, which record a Work id names, and filling `work up` arguments
//! from that record's plan.

use anyhow::{bail, Result};
use tend::ipc::Client;
use tend::work::{WorkRecord, WORK_BACKLOG_CAPABILITY};

/// Whether the daemon serves the backlog; used where it is optional.
pub async fn backlog_supported(client: &Client) -> bool {
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

/// Fill `tend work up` arguments the caller left out from the Work's plan,
/// if one exists. Explicit arguments always win and `--no-ticket` keeps the
/// saved issue out. The saved prompt is marked as the plan's
/// (`body_from_plan`): it is part of every launch prompt on every run, so a
/// re-run converges, and it never restarts or steers running agents.
pub fn fill_from_plan(args: &mut crate::work_up::UpArgs, record: &WorkRecord) {
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
    if args.body.is_none() && args.skill.is_none() && plan.prompt.is_some() {
        args.body.clone_from(&plan.prompt);
        args.body_from_plan = true;
    }
    if args.external.is_none() && !args.no_ticket {
        args.external.clone_from(&plan.external);
    }
}

/// The record for Work id `work`: the one in the named workspace, or the
/// only one when no workspace was named. Several without a workspace is an
/// error naming them, never a silent "none". With `planned_only`, records
/// without a plan are ignored.
pub fn find_record<'a>(
    records: &'a [WorkRecord],
    work: &str,
    workspace: Option<&str>,
    planned_only: bool,
) -> Result<Option<&'a WorkRecord>> {
    let work = crate::tmux_work::normalize_work_id(work)?;
    let workspace = workspace
        .map(crate::tmux_work::normalize_workspace_id)
        .transpose()?;
    let matching: Vec<&WorkRecord> = records
        .iter()
        .filter(|record| {
            (!planned_only || record.plan.is_some())
                && record.identity.work_id == work
                && workspace
                    .as_ref()
                    .is_none_or(|w| &record.identity.workspace_id == w)
        })
        .collect();
    match matching.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => bail!(
            "Work {work} is recorded in several workspaces ({}); pass --workspace",
            many.iter()
                .map(|record| record.identity.workspace_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
