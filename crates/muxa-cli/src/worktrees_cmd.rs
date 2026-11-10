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
    /// No blocker was found. Not a verdict that it may go: whether its Work
    /// is finished is not known yet.
    candidate: bool,
    /// What would stop a cleanup, in plain words.
    blockers: Vec<String>,
    /// The checked-out branch, or None when detached.
    current_branch: Option<String>,
    /// Size on disk, best effort.
    bytes: Option<u64>,
}

pub(crate) async fn run(args: Args) -> Result<()> {
    let receipts = muxa::worktree_receipt::list();
    let rows: Vec<Row> = tokio::task::spawn_blocking(move || {
        let panes = observe_panes();
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

/// Every pane's directory on every active backend, and whether every
/// backend answered in full.
struct Panes {
    cwds: Vec<PathBuf>,
    complete: bool,
}

fn observe_panes() -> Panes {
    let mut cwds = Vec::new();
    let mut complete = true;
    for backend in muxa::active_backends() {
        let observation = backend.observe_panes();
        complete &= matches!(
            observation.completeness,
            muxa::backend::ObservationCompleteness::Complete
        );
        cwds.extend(
            observation
                .panes
                .into_iter()
                .map(|pane| PathBuf::from(pane.current_path)),
        );
    }
    Panes { cwds, complete }
}

fn inspect(receipt: WorktreeReceipt, panes: &Panes) -> Row {
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
        candidate: reasons.is_empty(),
        blockers: reasons,
        current_branch,
        bytes,
        receipt,
    }
}

fn check(
    receipt: &WorktreeReceipt,
    panes: &Panes,
    reasons: &mut Vec<String>,
    current_branch: &mut Option<String>,
) {
    let path = &receipt.path;
    match receipt.still_owns() {
        Some(true) => {}
        Some(false) => {
            reasons.push("not the worktree muxa created (it was replaced or never marked)".into())
        }
        None => reasons.push("could not confirm it is the worktree muxa created".into()),
    }
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
        .cwds
        .iter()
        .any(|pane| lenient_canonical(pane).starts_with(path) || pane.starts_with(path))
    {
        reasons.push("a pane is working in it".into());
    } else if !panes.complete {
        reasons.push("could not see every pane, so one may be working in it".into());
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
        // Kept elsewhere means on a remote too; muxa does not fetch, so a
        // stale remote view only keeps more.
        let remote = git(&receipt.repo, &["branch", "--remotes", "--contains", &head]);
        if remote.as_deref().is_none_or(|out| out.trim().is_empty()) {
            reasons.push("its commits are not on any remote branch".into());
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
    let candidates: Vec<&Row> = rows.iter().filter(|row| row.candidate).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::worktree_receipt::ReceiptOrigin;
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

    fn all_panes_seen(cwds: Vec<PathBuf>) -> Panes {
        Panes {
            cwds,
            complete: true,
        }
    }

    /// A receipt as muxa writes it after creating `path`, mark included.
    fn created(
        repo: &Path,
        path: &Path,
        branch: Option<&str>,
        base: Option<&str>,
        start: Option<String>,
    ) -> WorktreeReceipt {
        let mut receipt = WorktreeReceipt {
            id: uuid::Uuid::new_v4().to_string(),
            repo: repo.to_path_buf(),
            path: path.to_path_buf(),
            branch: branch.map(Into::into),
            base: base.map(Into::into),
            start_head: None,
            origin: ReceiptOrigin::Work {
                workspace: None,
                work: "test".into(),
            },
            common_dir: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            state: ReceiptState::Creating,
        };
        receipt.mark_created(start).unwrap();
        receipt
    }

    /// A repository with a pushed `main` and a worktree on `feat` from it.
    fn fixture(root: &Path) -> (PathBuf, PathBuf) {
        let remote = root.join("remote.git");
        run_git(root, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "a").unwrap();
        run_git(&repo, &["add", "a"]);
        run_git(&repo, &["commit", "-qm", "a"]);
        run_git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_git(&repo, &["push", "-q", "-u", "origin", "main"]);
        let tree = root.join("wt");
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
        (repo, tree)
    }

    #[test]
    fn a_clean_pushed_unused_worktree_has_no_blocker() {
        let root = tempfile::tempdir().unwrap();
        let (repo, tree) = fixture(root.path());
        let receipt = created(&repo, &tree, Some("feat"), Some("main"), None);

        let row = inspect(receipt.clone(), &all_panes_seen(vec![]));
        assert!(row.candidate, "{:?}", row.blockers);

        // A pane inside it, or panes muxa could not see, block it.
        assert!(!inspect(receipt.clone(), &all_panes_seen(vec![tree.join("src")])).candidate);
        let unseen = Panes {
            cwds: vec![],
            complete: false,
        };
        let row = inspect(receipt.clone(), &unseen);
        assert!(
            row.blockers.iter().any(|r| r.contains("every pane")),
            "{:?}",
            row.blockers
        );

        // A local commit merged into main but on no remote blocks it.
        std::fs::write(tree.join("b"), "b").unwrap();
        run_git(&tree, &["add", "b"]);
        run_git(&tree, &["commit", "-qm", "b"]);
        run_git(&repo, &["merge", "-q", "--ff-only", "feat"]);
        let row = inspect(receipt.clone(), &all_panes_seen(vec![]));
        assert!(
            row.blockers.iter().any(|r| r.contains("remote")),
            "{:?}",
            row.blockers
        );

        // Ignored and untracked files block it.
        std::fs::write(tree.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir(tree.join("build")).unwrap();
        std::fs::write(tree.join("build/out"), "x").unwrap();
        let row = inspect(receipt, &all_panes_seen(vec![]));
        assert!(
            row.blockers.iter().any(|r| r.contains("ignored")),
            "{:?}",
            row.blockers
        );
        assert!(
            row.blockers.iter().any(|r| r.contains("uncommitted")),
            "{:?}",
            row.blockers
        );
    }

    #[test]
    fn a_worktree_made_again_at_the_same_path_is_not_muxas() {
        let root = tempfile::tempdir().unwrap();
        let (repo, tree) = fixture(root.path());
        let receipt = created(&repo, &tree, Some("feat"), Some("main"), None);
        run_git(&repo, &["worktree", "remove", tree.to_str().unwrap()]);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mine",
                tree.to_str().unwrap(),
                "main",
            ],
        );

        let row = inspect(receipt, &all_panes_seen(vec![]));
        assert!(!row.candidate);
        assert!(
            row.blockers
                .iter()
                .any(|r| r.contains("not the worktree muxa created")),
            "{:?}",
            row.blockers
        );
    }

    #[test]
    fn unfinished_receipts_and_detached_moves_are_blocked() {
        let root = tempfile::tempdir().unwrap();
        let (repo, _) = fixture(root.path());
        let start = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let run = root.path().join("run");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                run.to_str().unwrap(),
                &start,
            ],
        );

        let receipt = created(&repo, &run, None, None, Some(start.clone()));
        let row = inspect(receipt.clone(), &all_panes_seen(vec![]));
        assert!(row.candidate, "{:?}", row.blockers);

        let mut creating = receipt.clone();
        creating.state = ReceiptState::Creating;
        assert!(!inspect(creating, &all_panes_seen(vec![])).candidate);

        std::fs::write(run.join("b"), "b").unwrap();
        run_git(&run, &["add", "b"]);
        run_git(&run, &["commit", "-qm", "b"]);
        let row = inspect(receipt, &all_panes_seen(vec![]));
        assert!(
            row.blockers.iter().any(|r| r.contains("detached")),
            "{:?}",
            row.blockers
        );
    }
}
