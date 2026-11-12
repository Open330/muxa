//! `tend worktrees` — the git worktrees tend created on this host, what
//! would stop a cleanup from taking each, and the cleanup itself.
//!
//! Listing is read-only. A worktree is listed only when tend holds a
//! receipt for it (see `tend::worktree_receipt`); the blockers are
//! described in `tend::worktree_inventory`. `set-aside` moves worktrees
//! with no blocker into `.tend-set-aside` next to them, `restore` moves them
//! back, and `delete` removes set-aside ones for good (see
//! `tend::worktree_change`). Each prints its plan and changes nothing
//! without `--yes`.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Result};
use tend::worktree_change::{ChangeItem, ChangeRequest, Outcome};
use tend::worktree_inventory::WorktreeRow;
use tend::worktree_receipt::{ChangeAction, ReceiptState};
pub(crate) use tend_work::worktrees::begin_receipt;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: Option<Action>,
    /// Also list attempts that never finished or failed, and every deleted
    /// worktree.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Move worktrees with no blocker into `.tend-set-aside` next to them,
    /// with git. Nothing is deleted; `restore` moves them back.
    SetAside(ChangeArgs),
    /// Move set-aside worktrees back to where they were.
    Restore(ChangeArgs),
    /// Delete set-aside worktrees for good with `git worktree remove`.
    /// Their files cannot be restored; their last commit stays pinned
    /// under `refs/tend/set-aside/`.
    Delete(ChangeArgs),
}

#[derive(Debug, clap::Args)]
struct ChangeArgs {
    /// Receipt ids (or a unique prefix of 8 or more characters) or
    /// worktree paths, as `tend worktrees` lists them.
    #[arg(required = true)]
    targets: Vec<String>,
    /// Make the change; without it, only print the plan.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(args: Args) -> Result<()> {
    if let Some(action) = args.action {
        return change(action).await;
    }
    let rows = tokio::task::spawn_blocking(move || {
        tend::worktree_inventory::inventory(args.all, None).rows
    })
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

async fn change(action: Action) -> Result<()> {
    let (action, args) = match action {
        Action::SetAside(args) => (ChangeAction::SetAside, args),
        Action::Restore(args) => (ChangeAction::Restore, args),
        Action::Delete(args) => (ChangeAction::Delete, args),
    };
    let rows = tokio::task::spawn_blocking(|| tend::worktree_inventory::inventory(true, None).rows)
        .await?;
    let chosen = resolve(&rows, &args.targets)?;
    if !args.yes {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "action": action,
                    "changed": false,
                    "worktrees": chosen,
                }))?
            );
        } else {
            print!("{}", render_plan(action, &chosen));
        }
        return Ok(());
    }
    let request = ChangeRequest {
        op_id: uuid::Uuid::new_v4().to_string(),
        action,
        items: chosen
            .iter()
            .map(|row| ChangeItem {
                id: row.receipt.id.clone(),
                generation: row.receipt.generation,
                head: row.current_head.clone(),
                branch: row.current_branch.clone(),
            })
            .collect(),
        expected_node: tend::worktree_change::local_node(),
    };
    let outcomes = tokio::task::spawn_blocking(move || tend::worktree_change::change(&request))
        .await?
        .map_err(anyhow::Error::msg)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        for (row, outcome) in chosen.iter().zip(&outcomes) {
            let mark = match outcome.outcome {
                Outcome::Done => "done     ",
                Outcome::Refused => "refused  ",
                Outcome::Uncertain => "uncertain",
            };
            println!(
                "{mark}  {}  {}",
                row.receipt.path.display(),
                outcome.message
            );
        }
    }
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.outcome != Outcome::Done)
        .count();
    if failed > 0 {
        bail!("{failed} of {} worktree(s) not changed", outcomes.len());
    }
    Ok(())
}

/// Each target as exactly one receipted worktree: an id, an id prefix of 8
/// or more characters, or the path of a worktree tend made (where it is
/// now or where it was). Anything else is an error, never a guess.
fn resolve<'a>(rows: &'a [WorktreeRow], targets: &[String]) -> Result<Vec<&'a WorktreeRow>> {
    let mut chosen: Vec<&WorktreeRow> = Vec::new();
    for target in targets {
        let path = tend::paths::canonicalize(target).unwrap_or_else(|_| PathBuf::from(target));
        let matches: Vec<&WorktreeRow> = rows
            .iter()
            .filter(|row| {
                row.receipt.id == *target
                    || (target.len() >= 8 && row.receipt.id.starts_with(target.as_str()))
                    || row.receipt.path == path
                    || row
                        .receipt
                        .set_aside
                        .as_ref()
                        .is_some_and(|aside| aside.dest == path)
            })
            .collect();
        match matches.as_slice() {
            [row] => {
                if !chosen.iter().any(|seen| seen.receipt.id == row.receipt.id) {
                    chosen.push(row);
                }
            }
            [] => bail!("no worktree tend created matches {target}"),
            _ => bail!("{target} matches more than one worktree; use its full id"),
        }
    }
    Ok(chosen)
}

fn render_plan(action: ChangeAction, rows: &[&WorktreeRow]) -> String {
    let mut out = String::new();
    let (verb, note) = match action {
        ChangeAction::SetAside => (
            "Set aside",
            "Moves each folder with git into .tend-set-aside next to it. Nothing is deleted; `tend worktrees restore` moves it back.",
        ),
        ChangeAction::Restore => ("Restore", "Moves each set-aside folder back to where it was."),
        ChangeAction::Delete => (
            "Delete for good",
            "Deletes each folder with `git worktree remove`. Its local files and worktree git data cannot be restored; its last commit stays under refs/tend/set-aside/.",
        ),
    };
    let host = tend::worktree_change::local_node()
        .map_or_else(|| "this host".into(), |id| format!("this host ({id})"));
    let _ = writeln!(out, "{verb} on {host}:");
    for row in rows {
        let ready = match action {
            ChangeAction::SetAside => row.receipt.state == ReceiptState::Created && row.candidate,
            ChangeAction::Delete => row.receipt.state == ReceiptState::SetAside && row.candidate,
            ChangeAction::Restore => row.restorable,
        };
        let branch = row.current_branch.as_deref().unwrap_or("(detached)");
        let head = row
            .current_head
            .as_deref()
            .map_or("-", |head| &head[..head.len().min(12)]);
        let _ = writeln!(
            out,
            "{}  {}  {branch} {head}  gen {}  id {}",
            if ready { "ready  " } else { "blocked" },
            row.receipt.location().display(),
            row.receipt.generation,
            row.receipt.id
        );
        if !ready {
            for reason in &row.blockers {
                let _ = writeln!(out, "           - {reason}");
            }
        }
    }
    let _ = writeln!(out, "{note}");
    let _ = writeln!(out, "Nothing changed. Run again with --yes to do it.");
    out
}

fn render(rows: &[WorktreeRow]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "No worktrees tend created on this host.");
        return out;
    }
    let candidates: Vec<&WorktreeRow> = rows
        .iter()
        .filter(|row| row.candidate && row.receipt.state == ReceiptState::Created)
        .collect();
    let total: u64 = candidates.iter().filter_map(|row| row.bytes).sum();
    let _ = writeln!(
        out,
        "{} worktree(s) tend created; {} can be set aside ({}).",
        rows.len(),
        candidates.len(),
        human(total)
    );
    let _ = writeln!(
        out,
        "`tend worktrees set-aside <id>` moves one aside; `restore` and `delete` follow."
    );
    for row in rows {
        let mark = match (row.receipt.state, row.candidate) {
            (ReceiptState::Created, true) => "candidate ",
            (ReceiptState::SetAside, true) => "set aside ",
            (ReceiptState::SetAside, false) => "set aside*",
            (ReceiptState::Deleted, _) => "deleted   ",
            _ => "blocked   ",
        };
        let branch = row.current_branch.as_deref().unwrap_or("(detached)");
        let size = row.bytes.map_or_else(|| "-".into(), human);
        let _ = writeln!(
            out,
            "{mark}  {}  {branch}  {size}  {}",
            row.receipt.location().display(),
            row.receipt.id
        );
        for reason in &row.blockers {
            let _ = writeln!(out, "            - {reason}");
        }
    }
    out
}

#[allow(clippy::cast_precision_loss)] // a display size needs no more precision
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
