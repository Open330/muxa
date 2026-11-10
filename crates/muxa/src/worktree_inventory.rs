//! The git worktrees muxa created on this host, and what would stop a
//! cleanup from taking each. Shared by `muxa worktrees` and the daemon's
//! Fleet `worktree_inventory` operation, so Home can show every host.
//!
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

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::worktree_receipt::{ReceiptState, WorktreeReceipt};

/// One receipted worktree and its blockers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRow {
    #[serde(flatten)]
    pub receipt: WorktreeReceipt,
    /// No blocker was found. Not a verdict that it may go: whether its Work
    /// is finished is not known yet.
    pub candidate: bool,
    /// What would stop a cleanup, in plain words.
    pub blockers: Vec<String>,
    /// The checked-out branch, or None when detached.
    pub current_branch: Option<String>,
    /// Size on disk, best effort.
    pub bytes: Option<u64>,
}

/// The worktrees an inventory inspected, and how many receipts it had in
/// all, so a caller that set a limit can say what it left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub rows: Vec<WorktreeRow>,
    /// Receipts that matched, before any limit.
    pub total: usize,
}

/// Inspects receipted worktrees; blocking (git and pane scans), bounded by
/// [`SCAN_BUDGET`]. `all` also lists attempts that never finished or
/// failed. With a `limit`, trees still on disk come first, newest receipts
/// first, and the rest are counted in `total` but not inspected.
#[must_use]
pub fn inventory(all: bool, limit: Option<usize>) -> Inventory {
    let deadline = Instant::now() + SCAN_BUDGET;
    DEADLINE.with(|cell| cell.set(Some(deadline)));
    let panes = observe_panes();
    let mut receipts: Vec<WorktreeReceipt> = crate::worktree_receipt::list()
        .into_iter()
        .rev()
        .filter(|receipt| all || receipt.state == ReceiptState::Created)
        .collect();
    // Stable: newest first within each group.
    receipts.sort_by_key(|receipt| !receipt.path.exists());
    let total = receipts.len();
    let rows = receipts
        .into_iter()
        .take(limit.unwrap_or(usize::MAX))
        .map(|receipt| {
            if Instant::now() >= deadline {
                return out_of_time(receipt);
            }
            let mut row = inspect(receipt, &panes);
            if Instant::now() >= deadline && row.candidate {
                // A check cut short reads as a failure; say why.
                row.candidate = false;
                row.blockers.push(OUT_OF_TIME.into());
            }
            row
        })
        .collect();
    DEADLINE.with(|cell| cell.set(None));
    Inventory { rows, total }
}

/// The whole scan's budget: a host answers within it, however many trees
/// or however slow its disks. Trees left when it runs out are listed as not
/// checked.
pub const SCAN_BUDGET: Duration = Duration::from_secs(20);
/// One git or du call's budget; a child that overruns is killed.
pub const COMMAND_BUDGET: Duration = Duration::from_secs(5);
/// At most this many rows in a Fleet reply, so it stays far below the relay
/// frame ceiling; the reply says how many there were in all.
pub const FLEET_MAX_ROWS: usize = 300;
/// At most this much of a child's output is kept.
const MAX_OUTPUT: u64 = 1024 * 1024;
const OUT_OF_TIME: &str = "muxa ran out of time checking it";

thread_local! {
    /// The running scan's deadline, on the thread doing it.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn out_of_time(receipt: WorktreeReceipt) -> WorktreeRow {
    WorktreeRow {
        receipt,
        candidate: false,
        blockers: vec![OUT_OF_TIME.into()],
        current_branch: None,
        bytes: None,
    }
}

/// Runs `program args` and returns whether it exited 0 and up to 1 MiB of
/// its stdout, or None when it could not start, ran past its budget (the
/// smaller of [`COMMAND_BUDGET`] and what the scan has left), or left its
/// output open past that budget. The child runs in its own process group,
/// and the whole group is killed on expiry, so a helper that git or du
/// started and that holds the pipe open cannot outlive the budget either.
pub(crate) fn bounded(program: &str, args: &[&std::ffi::OsStr]) -> Option<(bool, String)> {
    use std::io::Read as _;
    let now = Instant::now();
    let budget = DEADLINE.with(Cell::get).map_or(COMMAND_BUDGET, |deadline| {
        deadline.saturating_duration_since(now).min(COMMAND_BUDGET)
    });
    if budget.is_zero() {
        return None;
    }
    let deadline = now + budget;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    // Read on another thread so a chatty child never blocks on a full pipe;
    // keep the first MiB and throw the rest away. The thread reports back
    // over a channel, so waiting for it is bounded too.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut pipe = stdout;
        let _ = (&mut pipe).take(MAX_OUTPUT).read_to_end(&mut kept);
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        let _ = sender.send(kept);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break None,
        }
    };
    let output = status.and_then(|_| {
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    });
    if let (Some(status), Some(output)) = (status, output) {
        Some((
            status.success(),
            String::from_utf8_lossy(&output).into_owned(),
        ))
    } else {
        kill_group(&mut child);
        None
    }
}

/// Kills the child and everything in its process group, then reaps it.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
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
    for backend in crate::active_backends() {
        let observation = backend.observe_panes();
        complete &= matches!(
            observation.completeness,
            crate::backend::ObservationCompleteness::Complete
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

fn inspect(receipt: WorktreeReceipt, panes: &Panes) -> WorktreeRow {
    let mut reasons = Vec::new();
    let mut current_branch = None;
    let mut bytes = None;
    match receipt.state {
        ReceiptState::Created => {}
        ReceiptState::Creating => {
            reasons.push("creation never finished; not proof muxa made it".into());
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
    WorktreeRow {
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
            reasons.push("not the worktree muxa created (it was replaced or never marked)".into());
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
        // Kept elsewhere means on a remote too, as far as this clone's
        // remote-tracking refs say. muxa does not fetch; those refs are
        // local evidence, not proof the server still holds the commit.
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
        if let Ok(resolved) = crate::paths::canonicalize(current) {
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
    let wanted = crate::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for block in listing.split("\n\n") {
        let mut lines = block.lines();
        let Some(tree) = lines.next().and_then(|line| line.strip_prefix("worktree ")) else {
            continue;
        };
        let tree = crate::paths::canonicalize(tree).unwrap_or_else(|_| PathBuf::from(tree));
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

/// stdout of a successful git command; None when git failed or overran.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command_line: Vec<&std::ffi::OsStr> = vec!["-C".as_ref(), dir.as_os_str()];
    command_line.extend(args.iter().map(std::ffi::OsStr::new));
    let (ok, stdout) = bounded("git", &command_line)?;
    ok.then_some(stdout)
}

fn size(path: &Path) -> Option<u64> {
    let (ok, text) = bounded("du", &["-sk".as_ref(), path.as_os_str()])?;
    if !ok {
        return None;
    }
    text.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_that_overruns_the_scan_is_killed() {
        DEADLINE.with(|cell| cell.set(Some(Instant::now() + Duration::from_millis(200))));
        let started = Instant::now();
        assert!(bounded("sleep", &["5".as_ref()]).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        DEADLINE.with(|cell| cell.set(None));
        assert_eq!(
            bounded("echo", &["hi".as_ref()]),
            Some((true, "hi\n".into()))
        );
    }

    #[test]
    fn a_helper_holding_the_pipe_cannot_outlive_the_budget() {
        // The direct child exits at once; its background helper keeps the
        // pipe open for 5 s.
        DEADLINE.with(|cell| cell.set(Some(Instant::now() + Duration::from_millis(300))));
        let started = Instant::now();
        assert!(bounded("sh", &["-c".as_ref(), "sleep 5 & exit 0".as_ref()]).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // And when the direct child itself overruns.
        DEADLINE.with(|cell| cell.set(Some(Instant::now() + Duration::from_millis(300))));
        let started = Instant::now();
        assert!(bounded("sh", &["-c".as_ref(), "sleep 5 & sleep 5".as_ref()]).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        DEADLINE.with(|cell| cell.set(None));
    }

    use crate::worktree_receipt::ReceiptOrigin;
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
