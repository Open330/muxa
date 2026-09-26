//! Supervision of the CLI worker that applies durable pipeline state.
//!
//! The worker uses the same claim/adoption path as interactive `work up`.
//! Stopping this supervisor must stop the worker, not the agents it already
//! launched in tmux. The store and the next reconciliation recover those panes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use muxa::pipeline_run::{PipelineRun, PipelineRunStore};
use muxa::work_control::{execute_work_command, WorkCommandError, WorkCommandLimits};
use tokio::sync::broadcast;

const SAFETY_SCAN: Duration = Duration::from_secs(30);
const FAILURE_BACKOFF: Duration = Duration::from_secs(5);
const LIMITS: WorkCommandLimits = WorkCommandLimits {
    timeout: Duration::from_secs(60),
    max_output_bytes: 64 * 1024,
};

pub(super) fn spawn(
    runs: Arc<PipelineRunStore>,
    socket: PathBuf,
    shutdown: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let program = std::env::var_os("MUXA_PIPELINE_CLI").map_or_else(
        || {
            std::env::current_exe()
                .ok()
                .map(|path| path.with_file_name("muxa"))
                .filter(|path| path.exists())
                .unwrap_or_else(|| PathBuf::from("muxa"))
        },
        PathBuf::from,
    );
    let shutdown = shutdown.subscribe();
    tokio::spawn(supervise(runs, program, socket, shutdown, LIMITS))
}

async fn supervise(
    runs: Arc<PipelineRunStore>,
    program: PathBuf,
    socket: PathBuf,
    mut shutdown: broadcast::Receiver<()>,
    limits: WorkCommandLimits,
) {
    let mut changes = runs.subscribe();
    let mut safety_scan = tokio::time::interval(SAFETY_SCAN);
    safety_scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // We scan on entry; consume the interval's immediate first tick.
    safety_scan.tick().await;
    loop {
        let pass = async {
            if runs.list().await.iter().any(PipelineRun::has_ready_alias) {
                reconcile(&program, &socket, limits).await
            } else {
                Ok(())
            }
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            result = pass => result,
        };
        if let Err(error) = result {
            tracing::warn!(program = %program.display(), %error, "pipeline reconcile failed");
            // Claims can notify us while the worker is failing. Neither those
            // revisions nor an overdue safety tick may bypass failure backoff.
            tokio::select! {
                biased;
                _ = shutdown.recv() => break,
                () = tokio::time::sleep(FAILURE_BACKOFF) => {}
            }
            continue;
        }
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            revision = changes.changed() => {
                if revision.is_err() {
                    break;
                }
            }
            _ = safety_scan.tick() => {}
        }
    }
}

async fn reconcile(
    program: &std::path::Path,
    socket: &std::path::Path,
    limits: WorkCommandLimits,
) -> Result<(), WorkCommandError> {
    let output = execute_work_command(
        program,
        &["work".into(), "reconcile".into(), "--all".into()],
        None,
        Some(socket),
        limits,
    )
    .await?;
    if output.exit_code != 0 {
        // Output capture is bounded already; keep the daemon log smaller still.
        let detail: String = output.stderr.trim().chars().take(2048).collect();
        return Err(WorkCommandError::Failed(format!(
            "pipeline worker exited with {}: {detail}",
            output.exit_code
        )));
    }
    tracing::debug!(program = %program.display(), "pipeline reconcile completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::pipeline::DesiredAgent;
    use muxa::pipeline_run::PipelineRunRegistration;
    use muxa::work::WorkIdentity;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Stdio;

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("muxa-worker");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn registration() -> PipelineRunRegistration {
        PipelineRunRegistration {
            identity: WorkIdentity::new("test", "work"),
            pipeline: "solo".into(),
            desired: vec![DesiredAgent {
                alias: "impl".into(),
                program: "codex".into(),
                role: None,
                task: None,
                prompt: None,
                options: Vec::new(),
                direction: None,
                after: Vec::new(),
            }],
            cwd: PathBuf::from("/tmp"),
            window_id: None,
            observed: Vec::new(),
            invalidate: Vec::new(),
        }
    }

    async fn wait_for_marker(path: &Path) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(path) {
                    if !value.is_empty() {
                        return value;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker did not start")
    }

    async fn assert_process_exited(pid: &str) {
        // The script execs sleep, so its PID is the supervised child, with
        // no shell descendants and no dependency on Linux /proc.
        assert!(pid.trim().parse::<u32>().unwrap() > 1);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let alive = tokio::process::Command::new("/bin/kill")
                    .args(["-0", pid.trim()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker survived cancellation or timeout");
    }

    #[tokio::test]
    async fn worker_receives_reconcile_arguments_socket_and_eof() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let binary = script(
            dir.path(),
            "test \"$*\" = 'work reconcile --all' || exit 2\nread unexpected && exit 3\nprintf '%s' \"$MUXA_SOCKET\" > \"$MUXA_SOCKET.started\"",
        );
        reconcile(&binary, &socket, LIMITS).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(socket.with_extension("started")).unwrap(),
            socket.to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn timeout_stops_and_reaps_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let binary = script(
            dir.path(),
            "printf '%s' \"$$\" > \"$MUXA_SOCKET.started\"\nexec sleep 60",
        );
        let result = reconcile(
            &binary,
            &socket,
            WorkCommandLimits {
                timeout: Duration::from_secs(1),
                ..LIMITS
            },
        )
        .await;
        assert!(matches!(result, Err(WorkCommandError::Timeout { .. })));
        let pid = wait_for_marker(&socket.with_extension("started")).await;
        assert_process_exited(&pid).await;
    }

    #[tokio::test]
    async fn shutdown_cancels_a_running_worker_without_waiting_for_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let binary = script(
            dir.path(),
            "printf '%s' \"$$\" > \"$MUXA_SOCKET.started\"\nexec sleep 60",
        );
        let runs = PipelineRunStore::in_memory();
        runs.register(registration()).await.unwrap();
        let (shutdown, _) = broadcast::channel(1);
        let task = tokio::spawn(supervise(
            runs,
            binary,
            socket.clone(),
            shutdown.subscribe(),
            LIMITS,
        ));
        let pid = wait_for_marker(&socket.with_extension("started")).await;
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown waited for the worker deadline")
            .unwrap();
        assert_process_exited(&pid).await;
    }

    #[tokio::test]
    async fn queued_shutdown_prevents_launching_a_ready_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let binary = script(dir.path(), "echo started > \"$MUXA_SOCKET.started\"");
        let runs = PipelineRunStore::in_memory();
        runs.register(registration()).await.unwrap();
        let (shutdown, _) = broadcast::channel(1);
        let receiver = shutdown.subscribe();
        shutdown.send(()).unwrap();
        supervise(runs, binary, socket.clone(), receiver, LIMITS).await;
        assert!(!socket.with_extension("started").exists());
    }

    #[tokio::test]
    async fn failures_back_off_despite_revisions_and_shutdown_interrupts_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let binary = script(
            dir.path(),
            "echo attempt >> \"$MUXA_SOCKET.started\"\necho failed >&2\nexit 1",
        );
        let runs = PipelineRunStore::in_memory();
        runs.register(registration()).await.unwrap();
        let (shutdown, _) = broadcast::channel(1);
        let task = tokio::spawn(supervise(
            runs.clone(),
            binary,
            socket.clone(),
            shutdown.subscribe(),
            LIMITS,
        ));
        wait_for_marker(&socket.with_extension("started")).await;
        for _ in 0..5 {
            runs.register(registration()).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            std::fs::read_to_string(socket.with_extension("started")).unwrap(),
            "attempt\n"
        );
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown waited for failure backoff")
            .unwrap();
    }

    #[tokio::test]
    async fn excessive_output_is_rejected_and_failed_exit_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let binary = script(dir.path(), "printf '0123456789abcdef'");
        let result = reconcile(
            &binary,
            &socket,
            WorkCommandLimits {
                max_output_bytes: 8,
                ..LIMITS
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(WorkCommandError::OutputTooLarge { .. })
        ));
        let binary = script(dir.path(), "echo 'cannot connect' >&2\nexit 7");
        let error = reconcile(&binary, &socket, LIMITS).await.unwrap_err();
        assert!(matches!(&error, WorkCommandError::Failed(_)));
        assert_eq!(
            error.to_string(),
            "pipeline worker exited with 7: cannot connect"
        );
    }
}
