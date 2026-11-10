//! Changes tend makes to the worktrees it created: set aside, restore, and
//! delete for good. Shared by `tend worktrees`, the daemon and Fleet relays,
//! so every path runs the same checks.
//!
//! Setting a worktree aside moves it with `git worktree move` into
//! `.tend-set-aside` next to it. Nothing is deleted: its files, ignored ones
//! included, and its git administrative data move with it, and restoring
//! moves it back. Deleting is a separate, explicit step for a worktree
//! already set aside: it runs `git worktree remove` without `--force`, and
//! the folder's local files and worktree-specific git data cannot be
//! brought back. Before either move or delete, its checked-out commit is
//! pinned under `refs/tend/set-aside/<id>/<commit>`, so the commit stays
//! reachable; tend never removes those refs.
//!
//! Every change takes one lock shared by all tend processes on the host,
//! checks the worktree again right before acting, and refuses when the
//! request names an older generation, a different branch or commit, or
//! another host. The change is written down, durably, before git runs; an
//! interrupted one is finished or undone from what is on disk at the next
//! change, never repeated, and stays "uncertain" when the disk does not
//! say. A pane or process could still start writing between the check and
//! the move; the move keeps whatever it wrote, and a delete checks again.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::worktree_inventory::{self, Liveness, Registration};
use crate::worktree_receipt::{
    ChangeAction, PendingChange, ReceiptState, SetAside, WorktreeReceipt,
};

/// One worktree a request names, as the caller last saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeItem {
    pub id: String,
    /// The receipt generation the caller saw.
    pub generation: u64,
    /// The full commit the caller saw checked out; required to set aside or
    /// delete.
    #[serde(default)]
    pub head: Option<String>,
    /// The branch the caller saw checked out; None for a detached checkout.
    #[serde(default)]
    pub branch: Option<String>,
}

/// A change to some of this host's worktrees. With no items it only
/// finishes or undoes interrupted changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequest {
    /// The caller's id for this request; a retry with the same id after a
    /// lost reply reads as done instead of failing.
    pub op_id: String,
    pub action: ChangeAction,
    #[serde(default)]
    pub items: Vec<ChangeItem>,
    /// The node id of the host the caller listed the worktrees on.
    #[serde(default)]
    pub expected_node: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Nothing was changed.
    Refused,
    /// tend could not tell whether the change happened.
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeOutcome {
    pub id: String,
    pub outcome: Outcome,
    pub message: String,
    /// The receipt's generation after this item, when known.
    #[serde(default)]
    pub generation: Option<u64>,
}

/// At most this many worktrees in one request.
pub const MAX_ITEMS: usize = 50;
/// A whole request, waiting for the lock included, ends within this; items
/// left when it runs out are refused untouched.
pub const CHANGE_BUDGET: Duration = Duration::from_secs(60);
/// How long a request waits for another change on this host to finish.
#[cfg(not(test))]
const LOCK_WAIT: Duration = Duration::from_secs(10);
#[cfg(test)]
const LOCK_WAIT: Duration = Duration::from_millis(300);
/// The folder next to a worktree that set-aside worktrees move into.
pub const SET_ASIDE_DIR: &str = ".tend-set-aside";

/// Applies `request` to this host's receipts. Blocking: git, pane and
/// process scans. Errors refuse the whole request before anything changed.
pub fn change(request: &ChangeRequest) -> Result<Vec<ChangeOutcome>, String> {
    let dir = crate::worktree_receipt::dir().ok_or("no tend data directory")?;
    let node = local_node();
    change_in(&dir, request, node.as_deref(), &Liveness::observe)
}

/// This host's node id, read without creating one.
#[must_use]
pub fn local_node() -> Option<String> {
    let path = crate::paths::default_node_id_file()?;
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.trim().to_string()).filter(|id| !id.is_empty())
}

pub(crate) fn change_in(
    dir: &Path,
    request: &ChangeRequest,
    node: Option<&str>,
    observe: &dyn Fn() -> Liveness,
) -> Result<Vec<ChangeOutcome>, String> {
    let started = Instant::now();
    let deadline = started + CHANGE_BUDGET;
    if request.op_id.trim().is_empty() || request.op_id.len() > 128 {
        return Err("a change needs an operation id of 1 to 128 characters".into());
    }
    if request.items.len() > MAX_ITEMS {
        return Err(format!("at most {MAX_ITEMS} worktrees at a time"));
    }
    if let Some(expected) = &request.expected_node {
        if node != Some(expected.as_str()) {
            return Err("this is not the host those worktrees were listed on; refresh".into());
        }
    }
    let _lock = take_lock(dir, (started + LOCK_WAIT).min(deadline))?;
    // Interrupted changes first, so every item starts from what is on disk.
    for receipt in crate::worktree_receipt::list_in(dir) {
        if receipt.pending.is_some() && Instant::now() < deadline {
            worktree_inventory::with_deadline(deadline, || reconcile(dir, receipt));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut outcomes = Vec::new();
    for item in &request.items {
        if !seen.insert(item.id.clone()) {
            continue;
        }
        if Instant::now() >= deadline {
            outcomes.push(refused(item, "tend ran out of time before this one"));
            continue;
        }
        let receipt = crate::worktree_receipt::list_in(dir)
            .into_iter()
            .find(|receipt| receipt.id == item.id);
        let outcome = match receipt {
            None => refused(item, "tend holds no receipt for it on this host"),
            Some(receipt) => worktree_inventory::with_deadline(deadline, || {
                apply(dir, receipt, item, request, observe)
            }),
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

fn refused(item: &ChangeItem, message: impl Into<String>) -> ChangeOutcome {
    ChangeOutcome {
        id: item.id.clone(),
        outcome: Outcome::Refused,
        message: message.into(),
        generation: None,
    }
}

fn outcome(
    receipt: &WorktreeReceipt,
    outcome: Outcome,
    message: impl Into<String>,
) -> ChangeOutcome {
    ChangeOutcome {
        id: receipt.id.clone(),
        outcome,
        message: message.into(),
        generation: Some(receipt.generation),
    }
}

/// The host-wide change lock, shared by the CLI, the daemon and relays.
fn take_lock(dir: &Path, until: Instant) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(dir.join(".change.lock"))
        .map_err(|error| format!("could not open the worktree change lock: {error}"))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err("another worktree change is running on this host; try again".into());
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("could not take the worktree change lock: {error}"));
            }
        }
    }
}

fn apply(
    dir: &Path,
    receipt: WorktreeReceipt,
    item: &ChangeItem,
    request: &ChangeRequest,
    observe: &dyn Fn() -> Liveness,
) -> ChangeOutcome {
    if receipt.last_op.as_deref() == Some(request.op_id.as_str())
        && receipt.generation == item.generation + 1
    {
        return outcome(&receipt, Outcome::Done, "already done");
    }
    if receipt.pending.is_some() {
        return outcome(
            &receipt,
            Outcome::Uncertain,
            "an earlier change did not finish and tend cannot tell where the worktree is; check it by hand",
        );
    }
    if receipt.generation != item.generation {
        return outcome(
            &receipt,
            Outcome::Refused,
            "it changed since it was listed; refresh and try again",
        );
    }
    match request.action {
        ChangeAction::SetAside => set_aside(dir, receipt, item, &request.op_id, observe),
        ChangeAction::Restore => restore(dir, receipt, &request.op_id),
        ChangeAction::Delete => delete(dir, receipt, item, &request.op_id, observe),
    }
}

/// Checks the worktree with fresh observations and that it still shows the
/// branch and commit the caller saw; returns that commit.
fn confirm(
    receipt: &WorktreeReceipt,
    item: &ChangeItem,
    observe: &dyn Fn() -> Liveness,
) -> Result<String, String> {
    let Some(head) = item.head.clone().filter(|head| !head.is_empty()) else {
        return Err("the request did not name the commit it saw".into());
    };
    let row = worktree_inventory::inspect(receipt.clone(), &observe());
    if !row.candidate {
        return Err(row.blockers.join("; "));
    }
    if row.current_head.as_deref() != Some(head.as_str()) || row.current_branch != item.branch {
        return Err(
            "its branch or commit changed since it was listed; refresh and try again".into(),
        );
    }
    Ok(head)
}

fn pin_name(id: &str, head: &str) -> String {
    format!("refs/tend/set-aside/{id}/{head}")
}

fn set_aside(
    dir: &Path,
    mut receipt: WorktreeReceipt,
    item: &ChangeItem,
    op_id: &str,
    observe: &dyn Fn() -> Liveness,
) -> ChangeOutcome {
    if receipt.state != ReceiptState::Created {
        return outcome(&receipt, Outcome::Refused, "it is not in place");
    }
    let head = match confirm(&receipt, item, observe) {
        Ok(head) => head,
        Err(why) => return outcome(&receipt, Outcome::Refused, why),
    };
    let dest = match set_aside_dest(&receipt.path, &receipt.id) {
        Ok(dest) => dest,
        Err(why) => return outcome(&receipt, Outcome::Refused, why),
    };
    let from = receipt.path.clone();
    receipt.pending = Some(PendingChange {
        op_id: op_id.into(),
        action: ChangeAction::SetAside,
        from: from.clone(),
        to: dest.clone(),
        head: head.clone(),
        branch: item.branch.clone(),
        started_at: OffsetDateTime::now_utc(),
    });
    if let Err(error) = receipt.save_in(dir) {
        return outcome(
            &receipt,
            Outcome::Refused,
            format!("could not write the change record: {error}"),
        );
    }
    if let Err(why) = pin(&receipt.repo, &pin_name(&receipt.id, &head), &head) {
        return undo_pending(dir, receipt, why);
    }
    let moved = run_git(
        &receipt.repo,
        &[
            "worktree".as_ref(),
            "move".as_ref(),
            from.as_os_str(),
            dest.as_os_str(),
        ],
    );
    finish(dir, receipt, moved, "git refused to move it")
}

fn restore(dir: &Path, mut receipt: WorktreeReceipt, op_id: &str) -> ChangeOutcome {
    let Some(aside) = receipt
        .set_aside
        .clone()
        .filter(|_| receipt.state == ReceiptState::SetAside)
    else {
        return outcome(&receipt, Outcome::Refused, "it is not set aside");
    };
    match std::fs::symlink_metadata(&receipt.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => {
            return outcome(
                &receipt,
                Outcome::Refused,
                format!("something is already at {}", receipt.path.display()),
            );
        }
    }
    if !receipt.path.parent().is_some_and(Path::is_dir) {
        return outcome(
            &receipt,
            Outcome::Refused,
            "the folder it came from is gone",
        );
    }
    if receipt.owns_at(&aside.dest) != Some(true) {
        return outcome(
            &receipt,
            Outcome::Refused,
            "the set-aside folder is not the worktree tend set aside",
        );
    }
    let head = head_at(&aside.dest).unwrap_or(aside.head.clone());
    let branch = branch_at(&aside.dest);
    receipt.pending = Some(PendingChange {
        op_id: op_id.into(),
        action: ChangeAction::Restore,
        from: aside.dest.clone(),
        to: receipt.path.clone(),
        head,
        branch,
        started_at: OffsetDateTime::now_utc(),
    });
    if let Err(error) = receipt.save_in(dir) {
        return outcome(
            &receipt,
            Outcome::Refused,
            format!("could not write the change record: {error}"),
        );
    }
    let to = receipt.path.clone();
    let moved = run_git(
        &receipt.repo,
        &[
            "worktree".as_ref(),
            "move".as_ref(),
            aside.dest.as_os_str(),
            to.as_os_str(),
        ],
    );
    finish(dir, receipt, moved, "git refused to move it back")
}

fn delete(
    dir: &Path,
    mut receipt: WorktreeReceipt,
    item: &ChangeItem,
    op_id: &str,
    observe: &dyn Fn() -> Liveness,
) -> ChangeOutcome {
    if receipt.state != ReceiptState::SetAside {
        return outcome(
            &receipt,
            Outcome::Refused,
            "only a set-aside worktree can be deleted",
        );
    }
    let head = match confirm(&receipt, item, observe) {
        Ok(head) => head,
        Err(why) => return outcome(&receipt, Outcome::Refused, why),
    };
    let dest = receipt.location().to_path_buf();
    receipt.pending = Some(PendingChange {
        op_id: op_id.into(),
        action: ChangeAction::Delete,
        from: dest.clone(),
        to: dest.clone(),
        head: head.clone(),
        branch: item.branch.clone(),
        started_at: OffsetDateTime::now_utc(),
    });
    if let Err(error) = receipt.save_in(dir) {
        return outcome(
            &receipt,
            Outcome::Refused,
            format!("could not write the change record: {error}"),
        );
    }
    // Pin the commit being deleted, which may be newer than the one pinned
    // when it was set aside; the older pin stays.
    if let Err(why) = pin(&receipt.repo, &pin_name(&receipt.id, &head), &head) {
        return undo_pending(dir, receipt, why);
    }
    let removed = run_git(
        &receipt.repo,
        &["worktree".as_ref(), "remove".as_ref(), dest.as_os_str()],
    );
    finish(dir, receipt, removed, "git refused to delete it")
}

/// `<parent>/.tend-set-aside/<name>.<id prefix>`, with the set-aside folder
/// a real directory in the same parent and nothing at the destination yet,
/// not even a dangling link.
fn set_aside_dest(path: &Path, id: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or("it has no parent folder to set it aside in")?;
    let name = path
        .file_name()
        .ok_or("it has no folder name")?
        .to_string_lossy()
        .into_owned();
    let aside = parent.join(SET_ASIDE_DIR);
    match std::fs::symlink_metadata(&aside) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => return Err(format!("{} is not a folder", aside.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&aside)
                .map_err(|error| format!("could not make {}: {error}", aside.display()))?;
        }
        Err(error) => return Err(error.to_string()),
    }
    let short: String = id.chars().take(8).collect();
    let dest = aside.join(format!("{name}.{short}"));
    match std::fs::symlink_metadata(&dest) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(dest),
        _ => Err(format!("something is already at {}", dest.display())),
    }
}

fn pin(repo: &Path, name: &str, head: &str) -> Result<(), String> {
    match run_git(repo, &["update-ref".as_ref(), name.as_ref(), head.as_ref()]) {
        Some(true) => Ok(()),
        _ => Err("could not pin its commit, so nothing was changed".into()),
    }
}

/// Clears a change that never reached git.
fn undo_pending(dir: &Path, mut receipt: WorktreeReceipt, why: String) -> ChangeOutcome {
    receipt.pending = None;
    match receipt.save_in(dir) {
        Ok(()) => outcome(&receipt, Outcome::Refused, why),
        Err(error) => outcome(
            &receipt,
            Outcome::Uncertain,
            format!("{why}; and the change record could not be cleared: {error}"),
        ),
    }
}

/// After git ran (or ran out of time): records the change when the disk
/// shows it happened, undoes the record when the disk shows it did not.
fn finish(
    dir: &Path,
    receipt: WorktreeReceipt,
    git_ok: Option<bool>,
    refused_message: &str,
) -> ChangeOutcome {
    let id = receipt.id.clone();
    let action = receipt.pending.as_ref().map(|p| p.action);
    match reconcile(dir, receipt) {
        Settled::Finished(receipt) => outcome(&receipt, Outcome::Done, done_message(action)),
        Settled::Undone(receipt) => {
            let message = if git_ok == Some(true) {
                "git reported success but the worktree did not change; nothing was recorded"
            } else {
                refused_message
            };
            outcome(&receipt, Outcome::Refused, message)
        }
        Settled::Unsaved(receipt, error) => outcome(
            &receipt,
            Outcome::Uncertain,
            format!("the change happened but its record could not be saved ({error}); tend checks it again next time"),
        ),
        Settled::Unknown(receipt) => ChangeOutcome {
            id,
            outcome: Outcome::Uncertain,
            message: "tend could not tell whether the change happened; it checks again before the next change".into(),
            generation: Some(receipt.generation),
        },
    }
}

fn done_message(action: Option<ChangeAction>) -> &'static str {
    match action {
        Some(ChangeAction::SetAside) => "set aside",
        Some(ChangeAction::Restore) => "restored",
        Some(ChangeAction::Delete) => "deleted",
        None => "done",
    }
}

enum Settled {
    Finished(WorktreeReceipt),
    Undone(WorktreeReceipt),
    Unsaved(WorktreeReceipt, std::io::Error),
    Unknown(WorktreeReceipt),
}

/// Where a worktree is, judged from git and the disk.
#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// Registered with git, carrying this receipt's mark.
    Here,
    /// Nothing on disk and nothing registered.
    Absent,
    /// Anything else: tend will not guess.
    Unclear,
}

fn place(receipt: &WorktreeReceipt, path: &Path) -> Place {
    let on_disk = match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Place::Unclear,
    };
    let registered = worktree_inventory::registration(&receipt.repo, path);
    match (on_disk, registered) {
        (false, Some(Registration::Missing)) => Place::Absent,
        (true, Some(Registration::Registered | Registration::Locked)) => {
            if receipt.owns_at(path) == Some(true) {
                Place::Here
            } else {
                Place::Unclear
            }
        }
        _ => Place::Unclear,
    }
}

/// Finishes or undoes `receipt`'s pending change from what is on disk,
/// saving the result. Never runs git commands that change anything.
fn reconcile(dir: &Path, mut receipt: WorktreeReceipt) -> Settled {
    let Some(pending) = receipt.pending.clone() else {
        return Settled::Finished(receipt);
    };
    let from = place(&receipt, &pending.from);
    let finished = match pending.action {
        ChangeAction::SetAside | ChangeAction::Restore => {
            let to = place(&receipt, &pending.to);
            match (from, to) {
                (Place::Absent, Place::Here) => Some(true),
                (Place::Here, Place::Absent) => Some(false),
                _ => None,
            }
        }
        ChangeAction::Delete => match from {
            // Gone as a delete leaves it; trust that only while the pin
            // that keeps its commit is there.
            Place::Absent
                if run_git(
                    &receipt.repo,
                    &[
                        "rev-parse".as_ref(),
                        "--verify".as_ref(),
                        "--quiet".as_ref(),
                        pin_name(&receipt.id, &pending.head).as_ref(),
                    ],
                ) == Some(true) =>
            {
                Some(true)
            }
            Place::Here => Some(false),
            _ => None,
        },
    };
    match finished {
        None => Settled::Unknown(receipt),
        Some(false) => {
            receipt.pending = None;
            match receipt.save_in(dir) {
                Ok(()) => Settled::Undone(receipt),
                Err(error) => Settled::Unsaved(receipt, error),
            }
        }
        Some(true) => {
            let pin = pin_name(&receipt.id, &pending.head);
            match pending.action {
                ChangeAction::SetAside => {
                    receipt.state = ReceiptState::SetAside;
                    receipt.set_aside = Some(SetAside {
                        dest: pending.to.clone(),
                        head: pending.head.clone(),
                        branch: pending.branch.clone(),
                        pins: vec![pin],
                        at: OffsetDateTime::now_utc(),
                    });
                }
                ChangeAction::Restore => {
                    receipt.state = ReceiptState::Created;
                    receipt.set_aside = None;
                }
                ChangeAction::Delete => {
                    receipt.state = ReceiptState::Deleted;
                    receipt.deleted_at = Some(OffsetDateTime::now_utc());
                    if let Some(aside) = receipt.set_aside.as_mut() {
                        if !aside.pins.contains(&pin) {
                            aside.pins.push(pin);
                        }
                    }
                }
            }
            receipt.pending = None;
            receipt.generation += 1;
            receipt.last_op = Some(pending.op_id.clone());
            match receipt.save_in(dir) {
                Ok(()) => Settled::Finished(receipt),
                Err(error) => Settled::Unsaved(receipt, error),
            }
        }
    }
}

fn run_git(repo: &Path, args: &[&std::ffi::OsStr]) -> Option<bool> {
    let mut command_line: Vec<&std::ffi::OsStr> = vec!["-C".as_ref(), repo.as_os_str()];
    command_line.extend_from_slice(args);
    worktree_inventory::bounded("git", &command_line).map(|(ok, _)| ok)
}

fn head_at(path: &Path) -> Option<String> {
    let args: [&std::ffi::OsStr; 4] = [
        "-C".as_ref(),
        path.as_os_str(),
        "rev-parse".as_ref(),
        "HEAD".as_ref(),
    ];
    let (ok, out) = worktree_inventory::bounded("git", &args)?;
    ok.then(|| out.trim().to_string())
        .filter(|head| !head.is_empty())
}

fn branch_at(path: &Path) -> Option<String> {
    let args: [&std::ffi::OsStr; 6] = [
        "-C".as_ref(),
        path.as_os_str(),
        "symbolic-ref".as_ref(),
        "--quiet".as_ref(),
        "--short".as_ref(),
        "HEAD".as_ref(),
    ];
    let (ok, out) = worktree_inventory::bounded("git", &args)?;
    ok.then(|| out.trim().to_string())
        .filter(|branch| !branch.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree_receipt::ReceiptOrigin;

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn quiet() -> Liveness {
        Liveness {
            panes: vec![],
            panes_complete: true,
            processes: Some(vec![]),
            runs: Ok(vec![]),
            dispatches: None,
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        receipts: PathBuf,
        repo: PathBuf,
        tree: PathBuf,
        receipt: WorktreeReceipt,
    }

    /// A pushed `main`, a worktree on `feat` from it, and its receipt.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let base = crate::paths::canonicalize(root.path()).unwrap();
        let remote = base.join("remote.git");
        run_git(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let repo = base.join("repo");
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
        let tree = base.join("wt");
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
        let receipts = base.join("receipts");
        let mut receipt = WorktreeReceipt {
            id: uuid::Uuid::new_v4().to_string(),
            repo: repo.clone(),
            path: tree.clone(),
            branch: Some("feat".into()),
            base: Some("main".into()),
            start_head: None,
            origin: ReceiptOrigin::Work {
                workspace: None,
                work: "test".into(),
            },
            common_dir: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            state: ReceiptState::Creating,
            generation: 0,
            set_aside: None,
            pending: None,
            last_op: None,
            deleted_at: None,
        };
        receipt.mark_created(None).unwrap();
        receipt.save_in(&receipts).unwrap();
        Fixture {
            _root: root,
            receipts,
            repo,
            tree,
            receipt,
        }
    }

    impl Fixture {
        fn current(&self) -> WorktreeReceipt {
            crate::worktree_receipt::list_in(&self.receipts)
                .into_iter()
                .find(|r| r.id == self.receipt.id)
                .unwrap()
        }

        fn item(&self) -> ChangeItem {
            let current = self.current();
            let at = current.location().to_path_buf();
            ChangeItem {
                id: current.id.clone(),
                generation: current.generation,
                head: Some(run_git(&at, &["rev-parse", "HEAD"])),
                branch: branch_at(&at),
            }
        }

        fn run(
            &self,
            op: &str,
            action: ChangeAction,
            items: Vec<ChangeItem>,
        ) -> Vec<ChangeOutcome> {
            let request = ChangeRequest {
                op_id: op.into(),
                action,
                items,
                expected_node: None,
            };
            change_in(&self.receipts, &request, None, &quiet).unwrap()
        }
    }

    #[test]
    fn set_aside_restore_and_delete_round_trip() {
        let f = fixture();
        // An ignored file would block it in place; set aside moves it intact.
        let out = f.run("op1", ChangeAction::SetAside, vec![f.item()]);
        assert_eq!(out[0].outcome, Outcome::Done, "{out:?}");
        let r = f.current();
        assert_eq!(r.state, ReceiptState::SetAside);
        assert_eq!(r.generation, 1);
        let dest = r.set_aside.clone().unwrap().dest;
        assert!(!f.tree.exists());
        assert!(dest.join("a").exists());
        assert!(dest.starts_with(f.tree.parent().unwrap().join(SET_ASIDE_DIR)));

        // A retry of the same request after a lost reply reads as done.
        let mut retry = f.item();
        retry.generation = 0;
        let out = f.run("op1", ChangeAction::SetAside, vec![retry.clone()]);
        assert_eq!(out[0].outcome, Outcome::Done, "{out:?}");
        // A different request from the old listing is refused.
        let out = f.run("op2", ChangeAction::SetAside, vec![retry]);
        assert_eq!(out[0].outcome, Outcome::Refused, "{out:?}");

        let out = f.run("op3", ChangeAction::Restore, vec![f.item()]);
        assert_eq!(out[0].outcome, Outcome::Done, "{out:?}");
        assert!(f.tree.join("a").exists());
        assert_eq!(f.current().state, ReceiptState::Created);
        assert_eq!(f.current().generation, 2);

        assert_eq!(
            f.run("op4", ChangeAction::SetAside, vec![f.item()])[0].outcome,
            Outcome::Done
        );
        let out = f.run("op5", ChangeAction::Delete, vec![f.item()]);
        assert_eq!(out[0].outcome, Outcome::Done, "{out:?}");
        let r = f.current();
        assert_eq!(r.state, ReceiptState::Deleted);
        assert!(!r.set_aside.unwrap().dest.exists());
        // The branch stays.
        run_git(&f.repo, &["rev-parse", "--verify", "feat"]);
    }

    #[test]
    fn a_branch_switch_at_the_same_commit_invalidates_the_request() {
        let f = fixture();
        let item = f.item();
        run_git(&f.tree, &["switch", "-q", "-c", "other"]);
        run_git(&f.repo, &["push", "-q", "origin", "other"]);
        let out = f.run("op", ChangeAction::SetAside, vec![item]);
        assert_eq!(out[0].outcome, Outcome::Refused, "{out:?}");
        assert!(out[0].message.contains("changed"), "{out:?}");
        assert!(f.tree.exists());
        assert!(f.current().pending.is_none());
    }

    #[test]
    fn a_newer_commit_made_while_set_aside_is_pinned_before_delete() {
        let f = fixture();
        assert_eq!(
            f.run("op1", ChangeAction::SetAside, vec![f.item()])[0].outcome,
            Outcome::Done
        );
        let dest = f.current().set_aside.unwrap().dest;
        // Someone commits there in detached mode; the commit is on no branch.
        run_git(&dest, &["switch", "-q", "--detach"]);
        std::fs::write(dest.join("b"), "b").unwrap();
        run_git(&dest, &["add", "b"]);
        run_git(&dest, &["commit", "-qm", "b"]);
        let newer = run_git(&dest, &["rev-parse", "HEAD"]);
        // Detached away from its start: delete refuses.
        let mut item = f.item();
        item.branch = None;
        let out = f.run("op2", ChangeAction::Delete, vec![item.clone()]);
        assert_eq!(out[0].outcome, Outcome::Refused, "{out:?}");

        // Pinning on its own: the newer commit survives gc after a delete
        // whose checks pass (simulated by pinning then removing).
        pin(&f.repo, &pin_name(&f.receipt.id, &newer), &newer).unwrap();
        run_git(
            &f.repo,
            &["worktree", "remove", "--force", dest.to_str().unwrap()],
        );
        run_git(&f.repo, &["gc", "-q", "--prune=now"]);
        run_git(&f.repo, &["cat-file", "-e", &format!("{newer}^{{commit}}")]);
    }

    #[test]
    fn interrupted_changes_finish_or_undo_from_what_is_on_disk() {
        // Crash after the record, before git: undone, nothing moved.
        let f = fixture();
        let mut r = f.current();
        let head = run_git(&f.tree, &["rev-parse", "HEAD"]);
        let dest = set_aside_dest(&f.tree, &r.id).unwrap();
        r.pending = Some(PendingChange {
            op_id: "crash".into(),
            action: ChangeAction::SetAside,
            from: f.tree.clone(),
            to: dest.clone(),
            head: head.clone(),
            branch: Some("feat".into()),
            started_at: OffsetDateTime::now_utc(),
        });
        r.save_in(&f.receipts).unwrap();
        assert!(f.run("check", ChangeAction::SetAside, vec![]).is_empty());
        let after = f.current();
        assert!(after.pending.is_none());
        assert_eq!(after.state, ReceiptState::Created);
        assert_eq!(after.generation, 0);

        // Crash after git moved it, before the record was finished.
        r.save_in(&f.receipts).unwrap();
        run_git(
            &f.repo,
            &[
                "worktree",
                "move",
                f.tree.to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );
        f.run("check", ChangeAction::SetAside, vec![]);
        let after = f.current();
        assert!(after.pending.is_none());
        assert_eq!(after.state, ReceiptState::SetAside);
        assert_eq!(after.generation, 1);
        assert_eq!(after.last_op.as_deref(), Some("crash"));
        // The lost reply's retry reads as done.
        let retry = ChangeItem {
            id: after.id.clone(),
            generation: 0,
            head: Some(head),
            branch: Some("feat".into()),
        };
        assert_eq!(
            f.run("crash", ChangeAction::SetAside, vec![retry])[0].outcome,
            Outcome::Done
        );

        // A place tend cannot explain stays pending and uncertain.
        let mut odd = f.current();
        odd.pending = Some(PendingChange {
            op_id: "odd".into(),
            action: ChangeAction::Restore,
            from: f.tree.parent().unwrap().join("nowhere"),
            to: f.tree.parent().unwrap().join("elsewhere"),
            head: "0".repeat(40),
            branch: None,
            started_at: OffsetDateTime::now_utc(),
        });
        odd.save_in(&f.receipts).unwrap();
        let out = f.run("next", ChangeAction::Restore, vec![f.item()]);
        assert_eq!(out[0].outcome, Outcome::Uncertain, "{out:?}");
        assert!(f.current().pending.is_some());
    }

    #[test]
    fn a_second_change_on_the_host_waits_then_gives_up() {
        let f = fixture();
        let held = take_lock(&f.receipts, Instant::now()).unwrap();
        let request = ChangeRequest {
            op_id: "op".into(),
            action: ChangeAction::SetAside,
            items: vec![f.item()],
            expected_node: None,
        };
        let error = change_in(&f.receipts, &request, None, &quiet).unwrap_err();
        assert!(error.contains("another worktree change"), "{error}");
        drop(held);
        assert!(change_in(&f.receipts, &request, None, &quiet).is_ok());
    }

    #[test]
    fn a_request_for_another_host_is_refused() {
        let f = fixture();
        let request = ChangeRequest {
            op_id: "op".into(),
            action: ChangeAction::SetAside,
            items: vec![f.item()],
            expected_node: Some("node-a".into()),
        };
        assert!(change_in(&f.receipts, &request, Some("node-b"), &quiet).is_err());
        assert!(f.tree.exists());
    }

    #[test]
    fn hidden_edits_and_live_work_block_it() {
        let f = fixture();
        run_git(&f.tree, &["update-index", "--assume-unchanged", "a"]);
        std::fs::write(f.tree.join("a"), "edited").unwrap();
        let row = worktree_inventory::inspect(f.current(), &quiet());
        assert!(
            row.blockers.iter().any(|b| b.contains("assume-unchanged")),
            "{:?}",
            row.blockers
        );
        let out = f.run("op", ChangeAction::SetAside, vec![f.item()]);
        assert_eq!(out[0].outcome, Outcome::Refused);
        assert_eq!(std::fs::read_to_string(f.tree.join("a")).unwrap(), "edited");

        let g = fixture();
        let busy = Liveness {
            processes: Some(vec![g.tree.join("src")]),
            ..quiet()
        };
        let row = worktree_inventory::inspect(g.current(), &busy);
        assert!(
            row.blockers.iter().any(|b| b.contains("process")),
            "{:?}",
            row.blockers
        );
    }

    fn run_json(work: &str, cwd: &Path, restart_pending: bool) -> crate::pipeline_run::PipelineRun {
        let at = serde_json::to_value(OffsetDateTime::UNIX_EPOCH).unwrap();
        serde_json::from_value(serde_json::json!({
            "identity": {"workspace_id": "ws", "work_id": work},
            "pipeline": "p",
            "desired": [{"alias": "impl", "program": "claude"}],
            "cwd": cwd,
            "generation": 1,
            "aliases": {"impl": {
                "alias": "impl", "status": "done", "generation": 1,
                "reconcile_pending": restart_pending,
                "updated_at": at
            }},
            "updated_at": at
        }))
        .unwrap()
    }

    #[test]
    fn work_and_dispatch_state_decide_whether_it_is_finished() {
        let f = fixture();
        // A Done alias still waiting on a restart is not finished.
        let live = Liveness {
            runs: Ok(vec![run_json("test", &f.tree, true)]),
            ..quiet()
        };
        let row = worktree_inventory::inspect(f.current(), &live);
        assert!(
            row.blockers.iter().any(|b| b.contains("restart pending")),
            "{:?}",
            row.blockers
        );
        let live = Liveness {
            runs: Ok(vec![run_json("test", &f.tree, false)]),
            ..quiet()
        };
        assert!(worktree_inventory::inspect(f.current(), &live).candidate);
        // Unreadable Work state blocks it.
        let live = Liveness {
            runs: Err("bad".into()),
            ..quiet()
        };
        assert!(!worktree_inventory::inspect(f.current(), &live).candidate);

        // A dispatch with no Work run and nothing running in the tree is
        // finished only when its record says so.
        let dispatches = f.receipts.parent().unwrap().join("dispatches");
        std::fs::create_dir_all(&dispatches).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut by_dispatch = f.current();
        by_dispatch.origin = ReceiptOrigin::Dispatch {
            dispatch_id: id.clone(),
            workspace: "ws".into(),
            work: "run".into(),
        };
        let live = || Liveness {
            dispatches: Some(dispatches.clone()),
            ..quiet()
        };
        let row = worktree_inventory::inspect(by_dispatch.clone(), &live());
        assert!(
            row.blockers.iter().any(|b| b.contains("no record")),
            "{:?}",
            row.blockers
        );
        std::fs::write(
            dispatches.join(format!("{id}.json")),
            r#"{"state":"launched"}"#,
        )
        .unwrap();
        let row = worktree_inventory::inspect(by_dispatch.clone(), &live());
        assert!(
            row.blockers
                .iter()
                .any(|b| b.contains("dispatch is launched")),
            "{:?}",
            row.blockers
        );
        // A finished Run for the same Work in another folder says nothing
        // about this dispatch; the Run for exactly its checkout does.
        let elsewhere = Liveness {
            runs: Ok(vec![run_json("run", &f.repo, false)]),
            dispatches: Some(dispatches.clone()),
            ..quiet()
        };
        let row = worktree_inventory::inspect(by_dispatch.clone(), &elsewhere);
        assert!(
            row.blockers
                .iter()
                .any(|b| b.contains("dispatch is launched")),
            "{:?}",
            row.blockers
        );
        let own = Liveness {
            runs: Ok(vec![run_json("run", &f.tree, false)]),
            dispatches: Some(dispatches.clone()),
            ..quiet()
        };
        assert!(worktree_inventory::inspect(by_dispatch.clone(), &own).candidate);
        std::fs::write(
            dispatches.join(format!("{id}.json")),
            r#"{"state":"failed"}"#,
        )
        .unwrap();
        assert!(!worktree_inventory::inspect(by_dispatch.clone(), &own).candidate);
        std::fs::write(
            dispatches.join(format!("{id}.json")),
            r#"{"state":"completed"}"#,
        )
        .unwrap();
        assert!(worktree_inventory::inspect(by_dispatch, &live()).candidate);
    }

    #[cfg(unix)]
    #[test]
    fn reading_work_state_leaves_the_file_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipeline-runs.json");
        std::fs::write(&path, r#"{"version":1,"runs":[]}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(crate::pipeline_run::read_runs(&path).unwrap().is_empty());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        std::fs::write(&path, r#"{"version":9,"runs":[]}"#).unwrap();
        assert!(crate::pipeline_run::read_runs(&path).is_err());
        assert!(crate::pipeline_run::read_runs(&dir.path().join("none"))
            .unwrap()
            .is_empty());
    }
}
