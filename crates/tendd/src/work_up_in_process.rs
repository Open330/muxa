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
    run_argv(&request.arguments(), socket)
        .await
        .map(|result| result.map_err(|error| WorkUpError::Failed(last_line(&error))))
}

/// Run a `["work", "up", …]` argv as the `tend` child would have: its JSON
/// document, or the error as the CLI's `main` printed it ([`printed`]).
/// `None` declines before any effect. Dropping the call stops it: no
/// further git, tmux or `prepare` command starts, as the child was killed
/// on drop, and an abandoned pass leaves its claims to recovery.
pub(crate) async fn run_argv(
    words: &[String],
    socket: PathBuf,
) -> Option<Result<serde_json::Value, String>> {
    let args = match tend_work::work_up::parse_up_argv(words) {
        Ok(args) => args,
        Err(error) => {
            tracing::warn!(%error, "in-process work up cannot read its arguments");
            return None;
        }
    };
    // Only the JSON document can be handed back.
    if !args.json {
        return None;
    }
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
    let (scope, _cancel_on_drop) = scope(&socket, tend::work_control::WORK_UP_TIMEOUT);
    let work = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("work up runtime: {error}"))?;
        tend_work::subprocess::with_scope(scope, || {
            runtime.block_on(async move {
                let client = tend::ipc::Client::new(socket);
                let result = tend_work::work_up::run_noninteractive(args, &config, &client).await;
                // As the CLI did after every `work up`, success or not.
                let _ = client.invalidate_agent_directory().await;
                let result = result.map_err(|error| printed(&error))?;
                serde_json::to_value(result).map_err(|error| error.to_string())
            })
        })
    });
    Some(match work.await {
        Ok(result) => result,
        Err(error) => Err(format!("work up task: {error}")),
    })
}

/// Sets a scope's cancel flag when the call that owns it is dropped.
pub(crate) struct CancelOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The child's limits for one in-process call: closed stdin, `TEND_SOCKET`,
/// its deadline, and a cancel flag the returned guard sets on drop.
pub(crate) fn scope(
    socket: &std::path::Path,
    budget: std::time::Duration,
) -> (tend_work::subprocess::Scope, CancelOnDrop) {
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    (
        tend_work::subprocess::Scope {
            socket: socket.to_path_buf(),
            deadline: std::time::Instant::now() + budget,
            cancel: Some(cancel.clone()),
        },
        CancelOnDrop(cancel),
    )
}

/// The config path the `tend` child chose: `--config` (never passed) or
/// `TEND_CONFIG`, else the default.
pub(crate) fn cli_config_path(
    env: Option<std::ffi::OsString>,
    default: Option<PathBuf>,
) -> Option<PathBuf> {
    env.filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or(default)
}

/// An error as the CLI's `main` printed it (`Error: {error:?}` without the
/// prefix): the message, then `Caused by:` and the chain. Unlike `{:?}`, it
/// never carries a backtrace. A backtrace appears when `RUST_BACKTRACE` is set
/// in tendd's environment, and its last line would then be a stack frame
/// instead of the error.
pub(crate) fn printed(error: &anyhow::Error) -> String {
    use std::fmt::Write as _;
    let mut text = error.to_string();
    let causes: Vec<String> = error.chain().skip(1).map(ToString::to_string).collect();
    if !causes.is_empty() {
        text.push_str("\n\nCaused by:");
        let numbered = causes.len() > 1;
        for (index, cause) in causes.iter().enumerate() {
            text.push('\n');
            if numbered {
                let _ = write!(text, "{index: >5}: ");
            } else {
                text.push_str("    ");
            }
            // anyhow indents a cause's later lines under its first.
            let indent = if numbered { "\n       " } else { "\n    " };
            text.push_str(&cause.replace('\n', indent));
        }
    }
    text
}

/// The child's error was the last line of its stderr, where `tend` printed
/// the error chain; keep that same detail.
pub(crate) fn last_line(text: &str) -> String {
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
    fn an_error_prints_as_anyhow_debug_without_a_backtrace() {
        let one = anyhow::anyhow!("bad cwd").context("no route");
        assert_eq!(printed(&one), "no route\n\nCaused by:\n    bad cwd");
        let two = anyhow::anyhow!("gone")
            .context("bad cwd")
            .context("no route");
        assert_eq!(
            printed(&two),
            "no route\n\nCaused by:\n    0: bad cwd\n    1: gone"
        );
        assert_eq!(printed(&anyhow::anyhow!("plain")), "plain");
        let multiline = anyhow::anyhow!("line one\nline two").context("outer");
        assert_eq!(
            printed(&multiline),
            format!("{multiline:?}")
                .split("\n\nStack backtrace:")
                .next()
                .unwrap()
        );
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
