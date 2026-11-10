//! `muxa worktrees` — the git worktrees muxa created on this host, and
//! whether each could be cleaned up.
//!
//! Read-only. A worktree is listed only when muxa holds a receipt saying it
//! created it (see `muxa::worktree_receipt`); a matching path is never taken
//! as proof. Each one is checked against the rules a cleanup would need:
//! still registered with git, nothing uncommitted, untracked or ignored in
//! it, no pane working inside it, and its commits kept elsewhere (a named
//! branch merged into its base, or a detached checkout still at the commit
//! it started from). Anything muxa cannot check counts against it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use muxa::worktree_receipt::{ReceiptState, WorktreeReceipt};
use serde::Serialize;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Also list attempts that never finished or failed.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct Row {
    #[serde(flatten)]
    receipt: WorktreeReceipt,
    /// Every rule held: a cleanup could take it.
    cleanable: bool,
    /// Why it is kept, in plain words; empty when cleanable.
    reasons: Vec<String>,
    /// The checked-out branch, or None when detached.
    current_branch: Option<String>,
    /// Size on disk, best effort.
    bytes: Option<u64>,
}

pub(crate) async fn run(args: Args) -> Result<()> {
    let receipts = muxa::worktree_receipt::list();
    let panes: Vec<PathBuf> = crate::navigation::all_panes()
        .await
        .into_iter()
        .map(|pane| PathBuf::from(pane.current_path))
        .collect();
    let rows: Vec<Row> = tokio::task::spawn_blocking(move || {
        receipts
            .into_iter()
            .filter(|receipt| args.all || receipt.state == ReceiptState::Created)
            .map(|receipt| inspect(receipt, &panes))
            .collect()
    })
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

fn inspect(receipt: WorktreeReceipt, panes: &[PathBuf]) -> Row {
    let mut reasons = Vec::new();
    let mut current_branch = None;
    let mut bytes = None;
    match receipt.state {
        ReceiptState::Created => {}
        ReceiptState::Creating => {
            reasons.push("creation never finished; not proof muxa made it".into())
        }
        ReceiptState::Failed => {
            reasons.push("creation failed".into());
        }
    }
    if receipt.state == ReceiptState::Created {
        if receipt.path.exists() {
            check(&receipt, panes, &mut reasons, &mut current_branch);
            bytes = size(&receipt.path);
        } else {
            reasons.push("already gone".into());
        }
    }
    Row {
        cleanable: reasons.is_empty(),
        reasons,
        current_branch,
        bytes,
        receipt,
    }
}

fn check(
    receipt: &WorktreeReceipt,
    panes: &[PathBuf],
    reasons: &mut Vec<String>,
    current_branch: &mut Option<String>,
) {
    let path = &receipt.path;
    match registration(&receipt.repo, path) {
        Some(Registration::Registered) => {}
        Some(Registration::Locked) => reasons.push("locked in git".into()),
        Some(Registration::Missing) => reasons.push("git no longer lists it as a worktree".into()),
        None => reasons.push("could not ask git about its worktrees".into()),
    }
    match git(
        path,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    ) {
        Some(status) => {
            if status.lines().any(|line| !line.starts_with("!!")) {
                reasons.push("has uncommitted or untracked changes".into());
            }
            if status.lines().any(|line| line.starts_with("!!")) {
                reasons.push("has ignored files (build output or local settings)".into());
            }
        }
        None => reasons.push("could not read its git status".into()),
    }
    if path.join(".gitmodules").exists() {
        reasons.push("has submodules".into());
    }
    if panes
        .iter()
        .any(|pane| lenient_canonical(pane).starts_with(path) || pane.starts_with(path))
    {
        reasons.push("a pane is working in it".into());
    }
    let head = git(path, &["rev-parse", "HEAD"]).map(|out| out.trim().to_string());
    *current_branch = git(path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|out| out.trim().to_string())
        .filter(|branch| !branch.is_empty());
    let Some(head) = head else {
        reasons.push("could not read its commit".into());
        return;
    };
    if current_branch.is_some() {
        let Some(base) = receipt.base.clone().or_else(|| default_base(&receipt.repo)) else {
            reasons.push("no base branch to compare with".into());
            return;
        };
        match git(
            &receipt.repo,
            &["merge-base", "--is-ancestor", &head, &base],
        ) {
            Some(_) => {}
            None => reasons.push(format!("its commits are not in {base}")),
        }
    } else {
        let start = receipt
            .start_head
            .as_deref()
            .and_then(|start| {
                git(
                    &receipt.repo,
                    &["rev-parse", &format!("{start}^{{commit}}")],
                )
            })
            .map(|out| out.trim().to_string());
        if start.as_deref() != Some(head.as_str()) {
            reasons.push("detached at a commit other than the one it started from".into());
        }
    }
}

/// `path` with its longest existing ancestor resolved, so a pane in
/// `/var/…/tree/src` matches a tree recorded as `/private/var/…/tree`
/// even when `src` is gone.
fn lenient_canonical(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        if let Ok(resolved) = muxa::paths::canonicalize(current) {
            return rest.iter().rev().fold(resolved, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

enum Registration {
    Registered,
    Locked,
    Missing,
}

fn registration(repo: &Path, path: &Path) -> Option<Registration> {
    let listing = git(repo, &["worktree", "list", "--porcelain"])?;
    let wanted = muxa::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for block in listing.split("\n\n") {
        let mut lines = block.lines();
        let Some(tree) = lines.next().and_then(|line| line.strip_prefix("worktree ")) else {
            continue;
        };
        let tree = muxa::paths::canonicalize(tree).unwrap_or_else(|_| PathBuf::from(tree));
        if tree == wanted {
            let locked = block
                .lines()
                .any(|line| line == "locked" || line.starts_with("locked "));
            return Some(if locked {
                Registration::Locked
            } else {
                Registration::Registered
            });
        }
    }
    Some(Registration::Missing)
}

fn default_base(repo: &Path) -> Option<String> {
    git(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .map(|out| out.trim().to_string())
    .filter(|out| !out.is_empty())
}

/// stdout of a successful git command; None when git failed.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn size(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("du")
        .arg("-sk")
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "No worktrees muxa created on this host.");
        return out;
    }
    let cleanable: Vec<&Row> = rows.iter().filter(|row| row.cleanable).collect();
    let total: u64 = cleanable.iter().filter_map(|row| row.bytes).sum();
    let _ = writeln!(
        out,
        "{} worktree(s) muxa created; {} could be cleaned up ({}).",
        rows.len(),
        cleanable.len(),
        human(total)
    );
    for row in rows {
        let mark = if row.cleanable { "ready" } else { "kept " };
        let branch = row.current_branch.as_deref().unwrap_or("(detached)");
        let size = row.bytes.map_or_else(|| "-".into(), human);
        let _ = writeln!(
            out,
            "{mark}  {}  {branch}  {size}",
            row.receipt.path.display()
        );
        for reason in &row.reasons {
            let _ = writeln!(out, "       - {reason}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn receipt(
        repo: &Path,
        path: &Path,
        branch: Option<&str>,
        base: Option<&str>,
        start: Option<String>,
    ) -> WorktreeReceipt {
        WorktreeReceipt {
            id: "r".into(),
            repo: repo.to_path_buf(),
            path: muxa::paths::canonicalize(path).unwrap(),
            branch: branch.map(Into::into),
            base: base.map(Into::into),
            start_head: start,
            origin: "work:test".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            state: ReceiptState::Created,
        }
    }

    #[test]
    fn only_clean_merged_unused_worktrees_are_cleanable() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "a").unwrap();
        run_git(&repo, &["add", "a"]);
        run_git(&repo, &["commit", "-qm", "a"]);
        let tree = root.path().join("wt");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                tree.to_str().unwrap(),
                "main",
            ],
        );

        // Clean and merged (no new commits): cleanable.
        let row = inspect(receipt(&repo, &tree, Some("feat"), Some("main"), None), &[]);
        assert!(row.cleanable, "{:?}", row.reasons);

        // A pane inside it keeps it.
        let row = inspect(
            receipt(&repo, &tree, Some("feat"), Some("main"), None),
            &[tree.join("src")],
        );
        assert!(!row.cleanable);

        // An ignored file keeps it.
        std::fs::write(tree.join(".gitignore"), "build/\n").unwrap();
        run_git(&tree, &["add", ".gitignore"]);
        run_git(&tree, &["commit", "-qm", "ignore"]);
        std::fs::create_dir(tree.join("build")).unwrap();
        std::fs::write(tree.join("build/out"), "x").unwrap();
        let row = inspect(receipt(&repo, &tree, Some("feat"), Some("main"), None), &[]);
        assert!(
            row.reasons.iter().any(|r| r.contains("ignored")),
            "{:?}",
            row.reasons
        );
        // ...and its new commit is not in main.
        assert!(
            row.reasons.iter().any(|r| r.contains("not in main")),
            "{:?}",
            row.reasons
        );

        // Untracked changes keep it.
        std::fs::remove_dir_all(tree.join("build")).unwrap();
        std::fs::write(tree.join("new"), "n").unwrap();
        let row = inspect(receipt(&repo, &tree, Some("feat"), Some("main"), None), &[]);
        assert!(
            row.reasons.iter().any(|r| r.contains("uncommitted")),
            "{:?}",
            row.reasons
        );
    }

    #[test]
    fn unfinished_receipts_and_detached_moves_are_kept() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "a").unwrap();
        run_git(&repo, &["add", "a"]);
        run_git(&repo, &["commit", "-qm", "a"]);
        let start = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let tree = root.path().join("run");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                tree.to_str().unwrap(),
                &start,
            ],
        );

        let row = inspect(receipt(&repo, &tree, None, None, Some(start.clone())), &[]);
        assert!(row.cleanable, "{:?}", row.reasons);

        let mut creating = receipt(&repo, &tree, None, None, Some(start.clone()));
        creating.state = ReceiptState::Creating;
        assert!(!inspect(creating, &[]).cleanable);

        std::fs::write(tree.join("b"), "b").unwrap();
        run_git(&tree, &["add", "b"]);
        run_git(&tree, &["commit", "-qm", "b"]);
        let row = inspect(receipt(&repo, &tree, None, None, Some(start)), &[]);
        assert!(
            row.reasons.iter().any(|r| r.contains("detached")),
            "{:?}",
            row.reasons
        );
    }
}
