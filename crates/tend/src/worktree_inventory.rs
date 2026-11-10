//! The git worktrees tend created on this host, and what would stop a
//! cleanup from taking each. Shared by `tend worktrees` and the daemon's
//! Fleet `worktree_inventory` operation, so Home can show every host.
//!
//! Read-only: it never changes a file, not even permissions. A worktree is
//! listed only when tend holds a receipt for it (see
//! `tend::worktree_receipt`), and it counts as tend's only while its git
//! administrative directory still carries that receipt's mark. Each is
//! checked for blockers: replaced or unregistered, locked, anything
//! uncommitted, untracked or ignored in it, files git is told not to look at
//! (assume-unchanged, skip-worktree, sparse checkout), a merge, rebase or
//! bisect in progress or settings and refs of its own, a pane or process
//! working inside it (or panes and processes tend could not see), its Work
//! or dispatch not finished, made within the last hour, and commits not
//! kept elsewhere (a named branch must be in its base and on a remote
//! branch; a detached checkout must still be at the commit it started
//! from). Anything tend cannot check is a blocker. No blocker makes it a
//! candidate for the next step (`tend::worktree_change`): set aside for a
//! worktree in place, deleted for good for one already set aside.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::pipeline_run::PipelineRun;
use crate::worktree_receipt::{ReceiptOrigin, ReceiptState, WorktreeReceipt};

/// One receipted worktree and its blockers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRow {
    #[serde(flatten)]
    pub receipt: WorktreeReceipt,
    /// No blocker was found for the next step: setting it aside when it is
    /// in place, deleting it when it is set aside.
    pub candidate: bool,
    /// What would stop that step, in plain words.
    pub blockers: Vec<String>,
    /// The checked-out branch, or None when detached.
    pub current_branch: Option<String>,
    /// The checked-out commit, full id; a change request must name it.
    #[serde(default)]
    pub current_head: Option<String>,
    /// A set-aside worktree whose original path is free to move back to.
    #[serde(default)]
    pub restorable: bool,
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

/// Deleted worktrees stay in the default listing this long.
const DELETED_SHOWN_FOR: time::Duration = time::Duration::days(7);
/// A worktree made more recently than this is not offered: its Work may not
/// have started yet. A heuristic on top of the checks, not evidence.
const MIN_AGE: time::Duration = time::Duration::hours(1);

/// Inspects receipted worktrees; blocking (git, pane and process scans),
/// bounded by [`SCAN_BUDGET`]. `all` also lists attempts that never
/// finished or failed, and every deleted one. With a `limit`, trees still
/// on disk come first, newest receipts first, and the rest are counted in
/// `total` but not inspected.
#[must_use]
pub fn inventory(all: bool, limit: Option<usize>) -> Inventory {
    let deadline = Instant::now() + SCAN_BUDGET;
    DEADLINE.with(|cell| cell.set(Some(deadline)));
    let live = Liveness::observe();
    let now = OffsetDateTime::now_utc();
    let mut receipts: Vec<WorktreeReceipt> = crate::worktree_receipt::list()
        .into_iter()
        .rev()
        .filter(|receipt| all || listed_by_default(receipt, now))
        .collect();
    // Stable: newest first within each group.
    receipts.sort_by_key(|receipt| !receipt.location().exists());
    let total = receipts.len();
    let rows = receipts
        .into_iter()
        .take(limit.unwrap_or(usize::MAX))
        .map(|receipt| {
            if Instant::now() >= deadline {
                return out_of_time(receipt);
            }
            let mut row = inspect(receipt, &live);
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

fn listed_by_default(receipt: &WorktreeReceipt, now: OffsetDateTime) -> bool {
    match receipt.state {
        ReceiptState::Created | ReceiptState::SetAside => true,
        ReceiptState::Deleted => {
            receipt.pending.is_some()
                || receipt
                    .deleted_at
                    .is_some_and(|at| now - at < DELETED_SHOWN_FOR)
        }
        ReceiptState::Creating | ReceiptState::Failed => false,
    }
}

/// Runs `work` with every bounded command limited to `deadline`.
pub fn with_deadline<T>(deadline: Instant, work: impl FnOnce() -> T) -> T {
    let previous = DEADLINE.with(|cell| cell.replace(Some(deadline)));
    let result = work();
    DEADLINE.with(|cell| cell.set(previous));
    result
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
const OUT_OF_TIME: &str = "tend ran out of time checking it";

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
        current_head: None,
        restorable: false,
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

/// What could be working in a worktree: panes, processes and Work.
pub(crate) struct Liveness {
    /// Every pane's directory on every active backend.
    pub(crate) panes: Vec<PathBuf>,
    /// Whether every backend answered in full.
    pub(crate) panes_complete: bool,
    /// Every visible process's working directory; None when tend could not
    /// list them.
    pub(crate) processes: Option<Vec<PathBuf>>,
    /// The daemon's durable Work runs, or why they could not be read.
    pub(crate) runs: Result<Vec<PipelineRun>, String>,
    /// Where dispatch records live (`<tend data>/dispatches/worker`).
    pub(crate) dispatches: Option<PathBuf>,
}

impl Liveness {
    pub(crate) fn observe() -> Self {
        let mut panes = Vec::new();
        let mut panes_complete = true;
        for backend in crate::active_backends() {
            let observation = backend.observe_panes();
            panes_complete &= matches!(
                observation.completeness,
                crate::backend::ObservationCompleteness::Complete
            );
            panes.extend(
                observation
                    .panes
                    .into_iter()
                    .map(|pane| PathBuf::from(pane.current_path)),
            );
        }
        let runs = match crate::paths::default_pipeline_run_file() {
            Some(path) => crate::pipeline_run::read_runs(&path).map_err(|error| error.to_string()),
            None => Err("no tend data directory".into()),
        };
        let dispatches = crate::paths::default_node_id_file()
            .and_then(|file| file.parent().map(Path::to_path_buf))
            .map(|data| data.join("dispatches").join("worker"));
        Self {
            panes,
            panes_complete,
            processes: process_cwds(),
            runs,
            dispatches,
        }
    }
}

/// Every visible process's working directory. Processes that end while
/// being read are skipped; another user's process tend may not read is
/// skipped too (it cannot write into this user's tree without this user's
/// permission bits, which a worktree tend made does not grant); one of this
/// user's own that cannot be read makes the list incomplete (None).
fn process_cwds() -> Option<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt as _;
        let me = std::fs::metadata("/proc/self").ok()?.uid();
        let mut cwds = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            match std::fs::read_link(entry.path().join("cwd")) {
                Ok(cwd) => cwds.push(cwd),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    match entry.metadata() {
                        Ok(meta) if meta.uid() != me => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        _ => return None,
                    }
                }
                Err(_) => {
                    if entry.path().exists() {
                        return None;
                    }
                }
            }
        }
        Some(cwds)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // lsof lists the processes this user may inspect; another user's
        // are left out, as above. -F0 ends every field with NUL, so a path
        // holding a newline still parses.
        let (_, out) = bounded(
            "lsof",
            &[
                "-nPw".as_ref(),
                "-d".as_ref(),
                "cwd".as_ref(),
                "-F0n".as_ref(),
            ],
        )?;
        let cwds: Vec<PathBuf> = out
            .split('\0')
            .map(|field| field.trim_start_matches('\n'))
            .filter_map(|field| field.strip_prefix('n'))
            .map(PathBuf::from)
            .collect();
        // lsof always reports at least itself; nothing means it failed.
        (!cwds.is_empty()).then_some(cwds)
    }
}

pub(crate) fn inspect(receipt: WorktreeReceipt, live: &Liveness) -> WorktreeRow {
    let mut reasons = Vec::new();
    let mut current_branch = None;
    let mut current_head = None;
    let mut restorable = false;
    let mut bytes = None;
    if let Some(pending) = &receipt.pending {
        reasons.push(format!(
            "an earlier {} did not finish; tend checks it before the next change",
            action_word(pending.action)
        ));
    }
    match receipt.state {
        ReceiptState::Created | ReceiptState::SetAside => {}
        ReceiptState::Creating => {
            reasons.push("creation never finished; not proof tend made it".into());
        }
        ReceiptState::Failed => {
            reasons.push("creation failed".into());
        }
        ReceiptState::Deleted => {
            reasons.push("deleted for good".into());
        }
    }
    match receipt.state {
        ReceiptState::Created => {
            if receipt.path.exists() {
                let state = check(&receipt, &receipt.path, live, &mut reasons);
                current_branch = state.branch;
                current_head = state.head;
                if OffsetDateTime::now_utc() - receipt.created_at < MIN_AGE {
                    reasons.push("made less than an hour ago".into());
                }
                bytes = size(&receipt.path);
            } else {
                reasons.push("already gone".into());
            }
        }
        ReceiptState::SetAside => {
            let dest = receipt.location().to_path_buf();
            if dest.exists() {
                let state = check(&receipt, &dest, live, &mut reasons);
                current_branch = state.branch;
                current_head = state.head;
                bytes = size(&dest);
                restorable = receipt.pending.is_none()
                    && std::fs::symlink_metadata(&receipt.path)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            } else {
                reasons.push("its set-aside folder is gone".into());
            }
        }
        _ => {}
    }
    WorktreeRow {
        candidate: reasons.is_empty(),
        blockers: reasons,
        current_branch,
        current_head,
        restorable,
        bytes,
        receipt,
    }
}

pub(crate) fn action_word(action: crate::worktree_receipt::ChangeAction) -> &'static str {
    match action {
        crate::worktree_receipt::ChangeAction::SetAside => "set-aside",
        crate::worktree_receipt::ChangeAction::Restore => "restore",
        crate::worktree_receipt::ChangeAction::Delete => "delete",
    }
}

/// What a check read about the checkout.
struct Checkout {
    head: Option<String>,
    branch: Option<String>,
}

/// Checks the worktree tend made, now at `path`.
fn check(
    receipt: &WorktreeReceipt,
    path: &Path,
    live: &Liveness,
    reasons: &mut Vec<String>,
) -> Checkout {
    match receipt.owns_at(path) {
        Some(true) => {}
        Some(false) => {
            reasons.push("not the worktree tend created (it was replaced or never marked)".into());
        }
        None => reasons.push("could not confirm it is the worktree tend created".into()),
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
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    ) {
        Some(status) => {
            let entries: Vec<&str> = status.split('\0').filter(|e| !e.is_empty()).collect();
            if entries.iter().any(|entry| !entry.starts_with("!!")) {
                reasons.push("has uncommitted or untracked changes".into());
            }
            if entries.iter().any(|entry| entry.starts_with("!!")) {
                reasons.push("has ignored files (build output or local settings)".into());
            }
        }
        None => reasons.push("could not read its git status".into()),
    }
    check_hidden_files(path, reasons);
    check_admin_state(path, reasons);
    if path.join(".gitmodules").exists() {
        reasons.push("has submodules".into());
    }
    check_activity(receipt, path, live, reasons);
    let head = git(path, &["rev-parse", "HEAD"])
        .map(|out| out.trim().to_string())
        .filter(|head| !head.is_empty());
    let branch = git(path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|out| out.trim().to_string())
        .filter(|branch| !branch.is_empty());
    let Some(head_id) = head.clone() else {
        reasons.push("could not read its commit".into());
        return Checkout { head, branch };
    };
    if branch.is_some() {
        let Some(base) = receipt.base.clone().or_else(|| default_base(&receipt.repo)) else {
            reasons.push("no base branch to compare with".into());
            return Checkout { head, branch };
        };
        match git(
            &receipt.repo,
            &["merge-base", "--is-ancestor", &head_id, &base],
        ) {
            Some(_) => {}
            None => reasons.push(format!("its commits are not in {base}")),
        }
        // Kept elsewhere means on a remote too, as far as this clone's
        // remote-tracking refs say. tend does not fetch; those refs are
        // local evidence, not proof the server still holds the commit.
        let remote = git(
            &receipt.repo,
            &["branch", "--remotes", "--contains", &head_id],
        );
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
        if start.as_deref() != Some(head_id.as_str()) {
            reasons.push("detached at a commit other than the one it started from".into());
        }
    }
    Checkout { head, branch }
}

/// Files git is told not to look at: an edit to one leaves `git status`
/// empty, and `git worktree remove` would delete it.
fn check_hidden_files(path: &Path, reasons: &mut Vec<String>) {
    match git(path, &["ls-files", "-v", "-z"]) {
        Some(listing) => {
            let hidden = listing.split('\0').any(|entry| {
                entry
                    .chars()
                    .next()
                    .is_some_and(|tag| tag.is_ascii_lowercase() || tag == 'S')
            });
            if hidden {
                reasons.push(
                    "has files git is told to ignore changes in (assume-unchanged or skip-worktree)"
                        .into(),
                );
            }
        }
        None => reasons.push("could not list its tracked files".into()),
    }
    let args: [&std::ffi::OsStr; 5] = [
        "-C".as_ref(),
        path.as_os_str(),
        "config".as_ref(),
        "--bool".as_ref(),
        "core.sparseCheckout".as_ref(),
    ];
    // Exit 1 with no output means the setting is unset.
    match bounded("git", &args) {
        Some((true, out)) if out.trim() == "true" => {
            reasons.push("uses a sparse checkout".into());
        }
        Some((true, _)) => {}
        Some((false, out)) if out.trim().is_empty() => {}
        _ => reasons.push("could not read its sparse-checkout setting".into()),
    }
}

/// A merge, rebase, cherry-pick, revert or bisect in progress, or settings
/// and refs of its own: all live in its administrative directory, which a
/// delete takes with it.
fn check_admin_state(path: &Path, reasons: &mut Vec<String>) {
    let Some(admin) = crate::worktree_receipt::admin_dir(path) else {
        reasons.push("could not find its git administrative directory".into());
        return;
    };
    let in_progress = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
    ];
    if in_progress.iter().any(|name| admin.join(name).exists()) {
        reasons.push("has a merge, rebase, cherry-pick or bisect in progress".into());
    }
    if admin.join("config.worktree").exists() {
        reasons.push("has git settings of its own (config.worktree)".into());
    }
    if admin.join("info/sparse-checkout").exists() {
        reasons.push("uses a sparse checkout".into());
    }
    let refs = admin.join("refs");
    if refs.exists() && has_files(&refs) != Some(false) {
        reasons.push("has refs of its own (refs/worktree or refs/bisect)".into());
    }
}

/// Whether a directory holds any file, at any depth; None when unreadable.
fn has_files(dir: &Path) -> Option<bool> {
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let kind = entry.file_type().ok()?;
        if kind.is_dir() {
            if has_files(&entry.path())? {
                return Some(true);
            }
        } else {
            return Some(true);
        }
    }
    Some(false)
}

/// Panes, processes, and the Work or dispatch that asked for it.
fn check_activity(
    receipt: &WorktreeReceipt,
    path: &Path,
    live: &Liveness,
    reasons: &mut Vec<String>,
) {
    let inside = |dir: &PathBuf| lenient_canonical(dir).starts_with(path) || dir.starts_with(path);
    // A pane works through processes, so a full process list covers panes
    // a backend could not list (cmux, for one, never lists them all). Only
    // when both are incomplete is it unknown.
    let pane_inside = live.panes.iter().any(inside);
    if pane_inside {
        reasons.push("a pane is working in it".into());
    }
    match &live.processes {
        Some(cwds) if cwds.iter().any(inside) => {
            if !pane_inside {
                reasons.push("a process is running in it".into());
            }
        }
        Some(_) => {}
        None if !live.panes_complete => reasons
            .push("could not see every pane or list processes, so one may be working in it".into()),
        None => reasons.push("could not list processes, so one may be running in it".into()),
    }
    let runs = match &live.runs {
        Ok(runs) => runs,
        Err(error) => {
            reasons.push(format!("could not read Work state: {error}"));
            return;
        }
    };
    let (workspace, work) = match &receipt.origin {
        ReceiptOrigin::Work { workspace, work } => (workspace.as_deref(), work.as_str()),
        ReceiptOrigin::Dispatch {
            workspace, work, ..
        } => (Some(workspace.as_str()), work.as_str()),
    };
    let matching: Vec<&PipelineRun> = runs
        .iter()
        .filter(|run| {
            let same_work = run.identity.work_id.eq_ignore_ascii_case(work)
                && workspace.is_none_or(|ws| run.identity.workspace_id.eq_ignore_ascii_case(ws));
            same_work || inside(&run.cwd) || lenient_canonical(&run.cwd).starts_with(&receipt.path)
        })
        .collect();
    if let Some(run) = matching.iter().find(|run| !run.finished()) {
        let what = run
            .unfinished_alias()
            .unwrap_or_else(|| "unfinished".into());
        reasons.push(format!("its Work is not finished ({what})"));
        return;
    }
    if let ReceiptOrigin::Dispatch { dispatch_id, .. } = &receipt.origin {
        // A dispatch counts as finished when its record says completed, or
        // when its Work run finished; anything else, a missing record
        // included, keeps it.
        if !matching.is_empty() {
            return;
        }
        match dispatch_state(live.dispatches.as_deref(), dispatch_id) {
            Ok(Some(state)) if state == "completed" => {}
            Ok(Some(state)) => reasons.push(format!("its dispatch is {state}")),
            Ok(None) => reasons.push("tend has no record of its dispatch".into()),
            Err(error) => reasons.push(format!("could not read its dispatch: {error}")),
        }
    }
}

fn dispatch_state(dir: Option<&Path>, dispatch_id: &str) -> Result<Option<String>, String> {
    let Some(dir) = dir else {
        return Err("no tend data directory".into());
    };
    let Ok(id) = uuid::Uuid::parse_str(dispatch_id) else {
        return Ok(None);
    };
    let body = match std::fs::read(dir.join(format!("{id}.json"))) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let record: serde_json::Value =
        serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    Ok(record
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
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

pub(crate) enum Registration {
    Registered,
    Locked,
    Missing,
}

pub(crate) fn registration(repo: &Path, path: &Path) -> Option<Registration> {
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

    fn all_panes_seen(cwds: Vec<PathBuf>) -> Liveness {
        Liveness {
            panes: cwds,
            panes_complete: true,
            processes: Some(vec![]),
            runs: Ok(vec![]),
            dispatches: None,
        }
    }

    /// A receipt as tend writes it after creating `path`, mark included.
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
            generation: 0,
            set_aside: None,
            pending: None,
            last_op: None,
            deleted_at: None,
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

        // A pane inside it, or panes tend could not see, block it.
        assert!(!inspect(receipt.clone(), &all_panes_seen(vec![tree.join("src")])).candidate);
        // Panes a backend could not list are covered by a full process
        // list; with neither it is unknown.
        let unseen = Liveness {
            panes_complete: false,
            ..all_panes_seen(vec![])
        };
        assert!(inspect(receipt.clone(), &unseen).candidate);
        let blind = Liveness {
            panes_complete: false,
            processes: None,
            ..all_panes_seen(vec![])
        };
        let row = inspect(receipt.clone(), &blind);
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
    fn a_worktree_made_again_at_the_same_path_is_not_tends() {
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
                .any(|r| r.contains("not the worktree tend created")),
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
