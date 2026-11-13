//! `work up` inside tendd. Before this, the daemon ran the sibling `tend`
//! binary, which called back over IPC. Now it runs the same `tend-work`
//! code itself, with the same inputs the child got:
//!
//! - the argv `WorkUpRequest::arguments` builds, parsed by the CLI's own
//!   `UpArgs`;
//! - the CLI's default config file (`paths::default_config_file`), not
//!   tendd's `--config`, exactly as the child read it;
//! - the daemon's own environment and working directory, which the child
//!   inherited;
//! - an IPC client on the socket the child had pinned as `TEND_SOCKET`;
//! - no prompts (the child's stdin was null).
//!
//! The work is synchronous in places (tmux and git subprocesses, blocking
//! IPC back to this daemon), so it runs on a blocking thread with its own
//! single-threaded runtime and never on the daemon's async workers.
//! Anything that fails before the first effect (unparseable argv, unreadable
//! config) declines, and the caller runs the child instead.
//! `TEND_WORK_UP_CHILD=1` forces the child.

use std::path::PathBuf;
use std::sync::Arc;
use tend::work_control::{InProcessWorkUp, WorkUpError, WorkUpRequest, WORK_UP_TIMEOUT};

pub(crate) fn install(own_socket: PathBuf) {
    let run: InProcessWorkUp = Arc::new(move |request: WorkUpRequest, socket: Option<PathBuf>| {
        let socket = socket.unwrap_or_else(|| own_socket.clone());
        Box::pin(async move { run(&request, socket).await })
    });
    tend::work_control::install_in_process_work_up(run);
}

async fn run(
    request: &WorkUpRequest,
    socket: PathBuf,
) -> Option<Result<serde_json::Value, WorkUpError>> {
    let args = match tend_work::work_up::parse_up_argv(&request.arguments()) {
        Ok(args) => args,
        Err(error) => {
            tracing::warn!(%error, "in-process work up cannot read its arguments");
            return None;
        }
    };
    let config_path = tend::paths::default_config_file();
    let config = match tend::Config::load_or_default(config_path.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "in-process work up cannot read the config");
            return None;
        }
    };
    let work = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| WorkUpError::Failed(format!("work up runtime: {error}")))?;
        runtime.block_on(async move {
            let client = tend::ipc::Client::new(socket);
            let result = tend_work::work_up::run_noninteractive(args, &config, &client)
                .await
                .map_err(|error| WorkUpError::Failed(last_line(&format!("{error:?}"))))?;
            serde_json::to_value(result).map_err(WorkUpError::InvalidJson)
        })
    });
    // Like the child's deadline, but a blocking thread cannot be killed: past
    // it the caller gets the timeout and the work finishes on its own.
    Some(match tokio::time::timeout(WORK_UP_TIMEOUT, work).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(WorkUpError::Failed(format!("work up task: {error}"))),
        Err(_) => Err(WorkUpError::Timeout),
    })
}

/// The child's error was the last line of its stderr, where `tend` printed
/// the error chain; keep that same detail.
fn last_line(text: &str) -> String {
    text.trim()
        .lines()
        .next_back()
        .unwrap_or("no error detail")
        .trim()
        .to_string()
}
