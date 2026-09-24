//! Workspace snapshots for Muxa.app and muxad's automatic snapshot.
//!
//! `muxa snapshot` / `muxa restore` (the CLI's `reload` module) are the only
//! code that knows the snapshot format, the restore planner and the resume
//! rules. Like [`crate::work_control`] for `work up`, the daemon does not grow
//! a second implementation: it runs those commands with `--json` as bounded
//! children and hands their documents through untouched, so a client decodes
//! exactly what the CLI prints.
//!
//! A restore can take minutes — every pane waits for its shell to come up —
//! so it runs as a tracked operation the client polls. Only one restore runs
//! at a time, and nothing else writes snapshots while it does: a snapshot of a
//! half-restored workspace is not one anybody wants back.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::work_control::{self, WorkCommandLimits, WorkCommandOutput};

/// Budget for listing, planning and deleting: they read a directory or ask
/// the server for its session names.
const QUICK: WorkCommandLimits = WorkCommandLimits {
    timeout: Duration::from_secs(30),
    max_output_bytes: 2 * 1024 * 1024,
};
/// Budget for taking a snapshot, which walks every pane's process table.
const SAVE: WorkCommandLimits = WorkCommandLimits {
    timeout: Duration::from_secs(60),
    max_output_bytes: 1024 * 1024,
};
/// Budget for a restore: up to twenty seconds per pane for its shell, on a
/// workspace that can have dozens of panes.
const RESTORE: WorkCommandLimits = WorkCommandLimits {
    timeout: Duration::from_secs(15 * 60),
    max_output_bytes: 4 * 1024 * 1024,
};
const MAX_RETAINED_OPERATIONS: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum MuxSnapshotError {
    #[error("{0:?} is not a snapshot id")]
    InvalidId(String),
    #[error("a snapshot restore is running; try again when it finishes")]
    RestoreRunning,
    #[error("a snapshot is being saved; try the restore again in a moment")]
    SaveRunning,
    #[error("no data directory to keep snapshots in")]
    NoSnapshotDir,
    #[error(transparent)]
    Command(#[from] work_control::WorkCommandError),
    #[error("{0}")]
    Failed(String),
    #[error("muxa printed something other than JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MuxSnapshotOperationState {
    Running,
    Succeeded,
    Failed,
}

/// A restore the client polls with `mux_snapshot_restore_status`.
#[derive(Debug, Clone, Serialize)]
pub struct MuxSnapshotOperation {
    pub operation_id: String,
    pub state: MuxSnapshotOperationState,
    /// The snapshot being restored.
    pub id: String,
    pub message: String,
    /// The `muxa restore --run --json` document — also on failure, when the
    /// CLI got far enough to print one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Default)]
struct Operations {
    values: BTreeMap<String, MuxSnapshotOperation>,
    order: VecDeque<String>,
}

#[derive(Debug)]
pub struct MuxSnapshotControl {
    socket_path: PathBuf,
    binary: PathBuf,
    /// `None` when there is no data directory; every request then fails
    /// with a clear message instead of guessing a location.
    root: Option<PathBuf>,
    next_id: AtomicU64,
    operations: tokio::sync::Mutex<Operations>,
    /// Captures in flight. Raised only while `operations` is locked and
    /// read by `restore` under the same lock, so a restore can never start
    /// in the middle of a capture — which would record a half-restored
    /// workspace. Lowered by [`SaveGuard`]'s drop, cancellation included.
    saving: AtomicUsize,
}

/// Lowers [`MuxSnapshotControl::saving`] when a capture ends, however it ends.
struct SaveGuard<'a>(&'a AtomicUsize);

impl Drop for SaveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Ids name a directory directly under the snapshots root and nothing else —
/// the same rule the CLI applies before it deletes anything.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl MuxSnapshotControl {
    /// The daemon's instance: the CLI next to muxad, the default snapshots
    /// directory, and children pointed back at this daemon's socket.
    pub fn new(socket_path: PathBuf) -> Arc<Self> {
        Self::with_parts(
            socket_path,
            work_control::resolve_muxa_binary(),
            crate::paths::default_snapshot_dir(),
        )
    }

    pub fn with_parts(socket_path: PathBuf, binary: PathBuf, root: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            socket_path,
            binary,
            root,
            next_id: AtomicU64::new(1),
            operations: tokio::sync::Mutex::new(Operations::default()),
            saving: AtomicUsize::new(0),
        })
    }

    /// `muxa snapshot --list --json`.
    pub async fn list(&self) -> Result<serde_json::Value, MuxSnapshotError> {
        self.json(&["snapshot", "--list", "--json"], QUICK).await
    }

    /// `muxa snapshot --json`: a manual snapshot of the server muxa's agents
    /// are on.
    pub async fn save(&self) -> Result<serde_json::Value, MuxSnapshotError> {
        let _saving = self.begin_save().await?;
        self.json(&["snapshot", "--json"], SAVE).await
    }

    /// muxad's periodic snapshot; the CLI decides whether anything changed
    /// and prunes old automatic ones.
    pub async fn auto(&self, keep: usize) -> Result<serde_json::Value, MuxSnapshotError> {
        let _saving = self.begin_save().await?;
        let keep = keep.to_string();
        self.json(
            &["snapshot", "--auto", "--keep-auto", &keep, "--json"],
            SAVE,
        )
        .await
    }

    /// The dry run: what a restore of `id` would create and skip. Always
    /// `--only-missing` — the app never adds panes to a live session.
    pub async fn plan(&self, id: &str) -> Result<serde_json::Value, MuxSnapshotError> {
        let dir = self.dir(id)?;
        let dir = dir.to_string_lossy();
        self.json(&["restore", &dir, "--only-missing", "--json"], QUICK)
            .await
    }

    /// `muxa snapshot --delete <id>`.
    pub async fn delete(&self, id: &str) -> Result<serde_json::Value, MuxSnapshotError> {
        self.dir(id)?;
        self.json(&["snapshot", "--delete", id, "--json"], QUICK)
            .await
    }

    /// Start `muxa restore <id> --only-missing --run --json` and return the
    /// running operation.
    pub async fn restore(
        self: &Arc<Self>,
        id: &str,
        layout_only: bool,
    ) -> Result<MuxSnapshotOperation, MuxSnapshotError> {
        let dir = self.dir(id)?;
        let mut operations = self.operations.lock().await;
        if operations
            .values
            .values()
            .any(|operation| operation.state == MuxSnapshotOperationState::Running)
        {
            return Err(MuxSnapshotError::RestoreRunning);
        }
        if self.saving.load(Ordering::SeqCst) > 0 {
            return Err(MuxSnapshotError::SaveRunning);
        }
        while operations.values.len() >= MAX_RETAINED_OPERATIONS {
            let Some(oldest) = operations.order.pop_front() else {
                break;
            };
            operations.values.remove(&oldest);
        }
        let operation_id = format!(
            "snapshot-restore-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let operation = MuxSnapshotOperation {
            operation_id: operation_id.clone(),
            state: MuxSnapshotOperationState::Running,
            id: id.to_owned(),
            message: "Restoring snapshot…".into(),
            result: None,
        };
        operations.order.push_back(operation_id.clone());
        operations
            .values
            .insert(operation_id.clone(), operation.clone());
        drop(operations);

        let mut args = vec![
            "restore".to_owned(),
            dir.to_string_lossy().into_owned(),
            "--only-missing".to_owned(),
            "--run".to_owned(),
            "--json".to_owned(),
        ];
        if layout_only {
            args.push("--layout-only".to_owned());
        }
        let control = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = control.run(&args, RESTORE).await;
            let mut operations = control.operations.lock().await;
            let Some(operation) = operations.values.get_mut(&operation_id) else {
                return;
            };
            match outcome {
                Ok(output) => {
                    // The CLI prints its report even when it stops early, so
                    // a failed restore still says which sessions it reached.
                    operation.result = serde_json::from_str(output.stdout.trim()).ok();
                    if output.exit_code == 0 {
                        operation.state = MuxSnapshotOperationState::Succeeded;
                        operation.message = "Snapshot restored".into();
                    } else {
                        operation.state = MuxSnapshotOperationState::Failed;
                        operation.message = failure_detail(&output);
                    }
                }
                Err(error) => {
                    operation.state = MuxSnapshotOperationState::Failed;
                    operation.message = error.to_string();
                }
            }
        });
        Ok(operation)
    }

    pub async fn status(&self, operation_id: &str) -> Option<MuxSnapshotOperation> {
        self.operations
            .lock()
            .await
            .values
            .get(operation_id)
            .cloned()
    }

    /// Refuses while a restore runs; otherwise counts this capture in, under
    /// the same lock `restore` checks it with.
    async fn begin_save(&self) -> Result<SaveGuard<'_>, MuxSnapshotError> {
        let operations = self.operations.lock().await;
        let running = operations
            .values
            .values()
            .any(|operation| operation.state == MuxSnapshotOperationState::Running);
        if running {
            return Err(MuxSnapshotError::RestoreRunning);
        }
        self.saving.fetch_add(1, Ordering::SeqCst);
        drop(operations);
        Ok(SaveGuard(&self.saving))
    }

    fn dir(&self, id: &str) -> Result<PathBuf, MuxSnapshotError> {
        if !valid_id(id) {
            return Err(MuxSnapshotError::InvalidId(id.to_owned()));
        }
        let root = self
            .root
            .as_deref()
            .ok_or(MuxSnapshotError::NoSnapshotDir)?;
        Ok(root.join(id))
    }

    async fn json(
        &self,
        args: &[&str],
        limits: WorkCommandLimits,
    ) -> Result<serde_json::Value, MuxSnapshotError> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let output = self.run(&args, limits).await?;
        if output.exit_code != 0 {
            return Err(MuxSnapshotError::Failed(failure_detail(&output)));
        }
        serde_json::from_str(output.stdout.trim()).map_err(MuxSnapshotError::InvalidJson)
    }

    async fn run(
        &self,
        args: &[String],
        limits: WorkCommandLimits,
    ) -> Result<WorkCommandOutput, MuxSnapshotError> {
        Ok(work_control::execute_work_command(
            &self.binary,
            args,
            None,
            Some(Path::new(&self.socket_path)),
            limits,
        )
        .await?)
    }
}

/// The last line of stderr, which is where the CLI's `Error: …` lands.
fn failure_detail(output: &WorkCommandOutput) -> String {
    output
        .stderr
        .trim()
        .lines()
        .next_back()
        .map(|line| line.trim_start_matches("Error: ").to_owned())
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| format!("muxa exited with status {}", output.exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in `muxa` that records its argv and answers like the CLI.
    fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("muxa");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}/argv\"\n{body}\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn argv(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("argv")).unwrap_or_default()
    }

    #[test]
    fn ids_cannot_leave_the_snapshots_directory() {
        assert!(valid_id("1790000000"));
        assert!(valid_id("1790000000-1"));
        for id in ["", ".", "..", "../x", "a/b", ".hidden", &"x".repeat(65)] {
            assert!(!valid_id(id), "{id:?}");
        }
    }

    /// A capture in flight holds restores off until it finishes, so a restore
    /// can never start in the middle of one.
    #[tokio::test]
    async fn a_restore_waits_for_a_capture_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_cli(dir.path(), "sleep 1\necho '{\"ok\":true}'");
        let control = MuxSnapshotControl::with_parts(
            dir.path().join("muxad.sock"),
            binary,
            Some(PathBuf::from("/snapshots")),
        );
        let saving = {
            let control = Arc::clone(&control);
            tokio::spawn(async move { control.save().await })
        };
        // Let the save get counted in before the restore asks.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let refused = control.restore("1790000000", false).await;
        assert!(
            matches!(refused, Err(MuxSnapshotError::SaveRunning)),
            "{refused:?}"
        );
        saving.await.unwrap().unwrap();
        assert_eq!(control.saving.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn requests_run_the_cli_with_json_and_only_missing() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_cli(dir.path(), r#"echo '{"ok":true}'"#);
        let control = MuxSnapshotControl::with_parts(
            dir.path().join("muxad.sock"),
            binary,
            Some(PathBuf::from("/snapshots")),
        );
        control.list().await.unwrap();
        control.save().await.unwrap();
        control.plan("1790000000").await.unwrap();
        control.delete("1790000000").await.unwrap();
        control.auto(7).await.unwrap();
        assert_eq!(
            argv(dir.path()),
            "snapshot --list --json\n\
             snapshot --json\n\
             restore /snapshots/1790000000 --only-missing --json\n\
             snapshot --delete 1790000000 --json\n\
             snapshot --auto --keep-auto 7 --json\n"
        );
        assert!(matches!(
            control.plan("../etc").await,
            Err(MuxSnapshotError::InvalidId(_))
        ));
    }

    #[tokio::test]
    async fn a_cli_failure_surfaces_its_error_line() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_cli(
            dir.path(),
            "echo 'Error: agents span several servers (a, b); pass --mux-socket to name one' >&2; exit 1",
        );
        let control = MuxSnapshotControl::with_parts(dir.path().join("muxad.sock"), binary, None);
        let error = control.save().await.unwrap_err().to_string();
        assert_eq!(
            error,
            "agents span several servers (a, b); pass --mux-socket to name one"
        );
    }

    async fn settle(control: &Arc<MuxSnapshotControl>, operation_id: &str) -> MuxSnapshotOperation {
        for _ in 0..200 {
            let operation = control.status(operation_id).await.unwrap();
            if operation.state != MuxSnapshotOperationState::Running {
                return operation;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("restore {operation_id} never settled");
    }

    #[tokio::test]
    async fn one_restore_at_a_time_and_nothing_saved_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let gate = dir.path().join("gate");
        let binary = fake_cli(
            dir.path(),
            &format!(
                "while [ ! -f '{}' ]; do sleep 0.05; done\necho '{{\"totals\":{{\"sessions_created\":1}}}}'",
                gate.display()
            ),
        );
        let control = MuxSnapshotControl::with_parts(
            dir.path().join("muxad.sock"),
            binary,
            Some(PathBuf::from("/snapshots")),
        );
        let running = control.restore("1790000000", true).await.unwrap();
        assert_eq!(running.state, MuxSnapshotOperationState::Running);
        assert!(matches!(
            control.restore("1790000000", false).await,
            Err(MuxSnapshotError::RestoreRunning)
        ));
        assert!(matches!(
            control.save().await,
            Err(MuxSnapshotError::RestoreRunning)
        ));
        assert!(matches!(
            control.auto(10).await,
            Err(MuxSnapshotError::RestoreRunning)
        ));

        std::fs::write(&gate, "").unwrap();
        let done = settle(&control, &running.operation_id).await;
        assert_eq!(done.state, MuxSnapshotOperationState::Succeeded);
        assert_eq!(
            done.result.unwrap()["totals"]["sessions_created"],
            serde_json::json!(1)
        );
        assert!(argv(dir.path())
            .contains("restore /snapshots/1790000000 --only-missing --run --json --layout-only"));
    }

    #[tokio::test]
    async fn a_failed_restore_keeps_the_partial_report() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_cli(
            dir.path(),
            "echo '{\"totals\":{\"sessions_failed\":1}}'; echo 'Error: creating session work' >&2; exit 1",
        );
        let control = MuxSnapshotControl::with_parts(
            dir.path().join("muxad.sock"),
            binary,
            Some(PathBuf::from("/snapshots")),
        );
        let running = control.restore("1000", false).await.unwrap();
        let done = settle(&control, &running.operation_id).await;
        assert_eq!(done.state, MuxSnapshotOperationState::Failed);
        assert_eq!(done.message, "creating session work");
        assert!(done.result.is_some());
    }
}
