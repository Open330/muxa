//! Workspace snapshots inside tendd. Before this, `MuxSnapshotControl` ran
//! `tend snapshot … --json` / `tend restore … --json` as bounded children.
//! Now it runs the same `tend_work::reload` code itself, with what the child
//! got:
//!
//! - the same argv, parsed by the CLI's own `SnapshotArgs` / `RestoreArgs`;
//! - the daemon's own environment and working directory, which the child
//!   inherited (the code reads no config file);
//! - an IPC client on the socket the child had pinned as `TEND_SOCKET`;
//! - for every process it starts, a closed stdin, `TEND_SOCKET` and the
//!   child's time budget (`tend_work::subprocess`), and, when the caller
//!   drops the call (tendd shutting down), the running command killed and no
//!   further one started, as `kill_on_drop` did to the child;
//! - after a restore, the agent-directory invalidation the CLI sent.
//!
//! The answer has the child's shape: exit code 0 or 1, the `--json` document
//! as stdout (also when a restore stops early, which printed its report
//! anyway), and `Error: ` plus the error chain on stderr, as the CLI's `main`
//! printed it (`work_up_in_process::printed`, which leaves out a backtrace).
//! An argv that does not parse, or one without `--json`, declines and runs
//! the child. `TEND_SNAPSHOT_CHILD=1` forces the child.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tend::mux_snapshot_control::InProcessSnapshot;
use tend::work_control::WorkCommandOutput;
use tend_work::reload::SnapshotRequest;

pub(crate) fn install() {
    let run: InProcessSnapshot =
        Arc::new(|args: Vec<String>, socket: PathBuf, budget: Duration| {
            Box::pin(async move { run(&args, socket, budget).await })
        });
    tend::mux_snapshot_control::install_in_process_snapshot(run);
}

/// Sets the scope's cancel flag when the call is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn run(args: &[String], socket: PathBuf, budget: Duration) -> Option<WorkCommandOutput> {
    let request = match SnapshotRequest::parse(args) {
        Ok(request) if request.json() => request,
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(%error, "in-process snapshot cannot read its arguments");
            return None;
        }
    };
    let restore = matches!(request, SnapshotRequest::Restore(_));
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let scope = tend_work::subprocess::Scope {
        socket: socket.clone(),
        deadline: std::time::Instant::now() + budget,
        cancel: Some(cancel),
    };
    let work = tokio::task::spawn_blocking(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return answer(Err(anyhow::anyhow!("snapshot runtime: {error}")), None),
        };
        let (result, document) = tend_work::subprocess::with_scope(scope, || {
            tend_work::reload::capture_json(|| {
                runtime.block_on(async {
                    // Queued behind other blocking work after the caller
                    // gave up: start nothing.
                    tend_work::subprocess::ensure_time_left()?;
                    let result = request.run(&socket).await;
                    if restore {
                        // As the CLI did after every `restore`, success or not.
                        let _ = tend::ipc::Client::new(socket.clone())
                            .invalidate_agent_directory()
                            .await;
                    }
                    result
                })
            })
        });
        answer(result, document)
    });
    Some(match work.await {
        Ok(output) => output,
        Err(error) => answer(Err(anyhow::anyhow!("snapshot task: {error}")), None),
    })
}

/// What the child would have exited with and printed.
fn answer(result: anyhow::Result<()>, document: Option<serde_json::Value>) -> WorkCommandOutput {
    let stdout = document
        .and_then(|document| serde_json::to_string_pretty(&document).ok())
        .map(|text| text + "\n")
        .unwrap_or_default();
    match result {
        Ok(()) => WorkCommandOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        },
        Err(error) => WorkCommandOutput {
            exit_code: 1,
            stdout,
            stderr: format!("Error: {}\n", crate::work_up_in_process::printed(&error)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn the_daemon_argvs_parse_back() {
        for words in [
            &["snapshot", "--list", "--json"][..],
            &["snapshot", "--json"],
            &["snapshot", "--auto", "--keep-auto", "10", "--json"],
            &["snapshot", "--delete", "1794000000", "--json"],
            &["restore", "/s/1", "--only-missing", "--json"],
            &["restore", "/s/1", "--run", "--json", "--continue=carry on"],
            &["restore", "/s/1", "--run", "--json", "--layout-only"],
        ] {
            let request = SnapshotRequest::parse(&argv(words)).unwrap();
            assert!(request.json(), "{words:?}");
        }
        assert!(!SnapshotRequest::parse(&argv(&["snapshot"])).unwrap().json());
        assert!(SnapshotRequest::parse(&argv(&["work", "up"])).is_err());
    }

    #[test]
    fn a_failure_reads_as_the_cli_printed_it() {
        let output = answer(
            Err(anyhow::anyhow!("no server").context("restore failed")),
            Some(serde_json::json!({"run": true})),
        );
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.stdout.trim(), "{\n  \"run\": true\n}");
        assert!(output.stderr.starts_with("Error: restore failed"));
        assert_eq!(
            output.stderr.trim().lines().next_back(),
            Some("    no server")
        );
    }
}
