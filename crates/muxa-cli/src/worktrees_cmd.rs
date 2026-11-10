//! `muxa worktrees` — the git worktrees muxa created on this host, and what
//! would stop a cleanup from taking each.
//!
//! Read-only. A worktree is listed only when muxa holds a receipt for it
//! (see `muxa::worktree_receipt`), and it counts as muxa's only while its
//! git administrative directory still carries that receipt's mark. Each is
//! checked for blockers: replaced or unregistered, locked, anything
//! uncommitted, untracked or ignored in it, a pane working inside it (or
//! panes muxa could not see), and commits not kept elsewhere (a named branch
//! must be in its base and on a remote branch; a detached checkout must
//! still be at the commit it started from). Anything muxa cannot check is a
//! blocker. A worktree with no blocker is only a candidate: muxa does not
//! yet know whether its Work is finished, and nothing is removed.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use muxa::worktree_inventory::WorktreeRow;
use muxa::worktree_receipt::WorktreeReceipt;

/// Writes the receipt for a worktree muxa is about to create. A receipt
/// that cannot be written only costs cleanup its proof, so failure is
/// logged, not raised. Unit tests make worktrees in temporary directories;
/// their receipts would land in the user's muxa data, so tests write none.
pub(crate) fn begin_receipt(
    repo: &Path,
    path: &Path,
    branch: Option<String>,
    base: Option<String>,
    origin: muxa::worktree_receipt::ReceiptOrigin,
) -> Option<WorktreeReceipt> {
    if cfg!(test) {
        return None;
    }
    WorktreeReceipt::begin(repo, path, branch, base, origin)
        .map_err(|error| tracing::warn!(%error, "worktree receipt not written"))
        .ok()
}

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Also list attempts that never finished or failed.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(args: Args) -> Result<()> {
    let rows =
        tokio::task::spawn_blocking(move || muxa::worktree_inventory::inventory(args.all)).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

fn render(rows: &[WorktreeRow]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "No worktrees muxa created on this host.");
        return out;
    }
    let candidates: Vec<&WorktreeRow> = rows.iter().filter(|row| row.candidate).collect();
    let total: u64 = candidates.iter().filter_map(|row| row.bytes).sum();
    let _ = writeln!(
        out,
        "{} worktree(s) muxa created; {} with no blocker ({}).",
        rows.len(),
        candidates.len(),
        human(total)
    );
    let _ = writeln!(
        out,
        "Read-only: muxa does not yet know whether a Work is finished, and removes nothing."
    );
    for row in rows {
        let mark = if row.candidate {
            "candidate"
        } else {
            "blocked  "
        };
        let branch = row.current_branch.as_deref().unwrap_or("(detached)");
        let size = row.bytes.map_or_else(|| "-".into(), human);
        let _ = writeln!(
            out,
            "{mark}  {}  {branch}  {size}",
            row.receipt.path.display()
        );
        for reason in &row.blockers {
            let _ = writeln!(out, "           - {reason}");
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
