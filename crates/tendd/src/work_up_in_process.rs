//! `work up` inside tendd. Before this, the daemon ran the sibling `tend`
//! binary, which called back over IPC. Now it runs the same `tend-work`
//! code itself, with the same inputs the child got:
//!
//! - the argv `WorkUpRequest::arguments` builds, parsed by the CLI's own
//!   `UpArgs`;
//! - the config file the child's CLI chose: `TEND_CONFIG` (inherited),
//!   else the default path, and never tendd's own `--config`;
//! - the daemon's own environment and working directory, which the child
//!   inherited;
//! - an IPC client on the socket the child had pinned as `TEND_SOCKET`;
//! - no prompts;
//! - for every git, tmux and `prepare` process it starts, what the child's
//!   own processes had: a closed stdin, `TEND_SOCKET`, and the 600 s
//!   deadline, enforced by killing the process group and refusing new
//!   commands once it has passed (`tend_work::subprocess`);
//! - the agent-directory invalidation the CLI sent after every `work up`.
//!
//! The work is synchronous in places (tmux and git subprocesses, blocking
//! IPC back to this daemon), so it runs on a blocking thread with its own
//! single-threaded runtime and never on the daemon's async workers.
//! Anything that fails before the first effect (unparseable argv, unreadable
//! config, or a ticket lookup, which starts an agent outside that scope)
//! declines, and the caller runs the child instead. There is no outer
//! timeout: the operation stays running, and counted, until the work really
//! ends, which the subprocess deadline bounds.
//! `TEND_WORK_UP_CHILD=1` forces the child.

use std::path::PathBuf;
use std::sync::Arc;
use tend::work_control::{InProcessWorkUp, WorkUpError, WorkUpRequest};

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
    // A ticket lookup starts a headless agent through `tend::ask`, outside
    // the subprocess scope below, so it would miss the socket the child
    // pinned for its hooks. Leave those to the child.
    if args.external.is_some() && !args.no_ticket {
        return None;
    }
    let config_path = cli_config_path(
        std::env::var_os("TEND_CONFIG"),
        tend::paths::default_config_file(),
    );
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
        let scope = tend_work::subprocess::Scope {
            socket: socket.clone(),
            deadline: std::time::Instant::now() + tend::work_control::WORK_UP_TIMEOUT,
        };
        tend_work::subprocess::with_scope(scope, || {
            runtime.block_on(async move {
                let client = tend::ipc::Client::new(socket);
                let result = tend_work::work_up::run_noninteractive(args, &config, &client).await;
                // As the CLI did after every `work up`, success or not.
                let _ = client.invalidate_agent_directory().await;
                let result = result
                    .map_err(|error| WorkUpError::Failed(last_line(&format!("{error:?}"))))?;
                serde_json::to_value(result).map_err(WorkUpError::InvalidJson)
            })
        })
    });
    Some(match work.await {
        Ok(result) => result,
        Err(error) => Err(WorkUpError::Failed(format!("work up task: {error}"))),
    })
}

/// The config path the `tend` child chose: `--config` (never passed) or
/// `TEND_CONFIG`, else the default.
fn cli_config_path(env: Option<std::ffi::OsString>, default: Option<PathBuf>) -> Option<PathBuf> {
    env.filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or(default)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_the_one_the_cli_child_read() {
        let default = Some(PathBuf::from("/home/u/.config/tend/config.toml"));
        assert_eq!(
            cli_config_path(Some("/custom/tend.toml".into()), default.clone()),
            Some(PathBuf::from("/custom/tend.toml"))
        );
        assert_eq!(cli_config_path(Some("".into()), default.clone()), default);
        assert_eq!(cli_config_path(None, default.clone()), default);
    }

    #[test]
    fn the_error_detail_is_the_last_line_the_cli_printed() {
        assert_eq!(
            last_line("no route\n\nCaused by:\n    0: bad cwd\n"),
            "0: bad cwd"
        );
        assert_eq!(last_line(""), "no error detail");
    }
}
