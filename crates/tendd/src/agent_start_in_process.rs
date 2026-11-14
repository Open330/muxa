//! `agent start` inside tendd. Before this, the daemon ran the sibling
//! `tend` binary (`tend agent start --json …`), which called back over IPC.
//! Now it runs the same `tend-work` code itself, with what the child got:
//!
//! - the argv `AgentStartRequest::arguments` builds, parsed by the CLI's own
//!   `StartArgs`;
//! - the config file the child's CLI chose (`TEND_CONFIG`, else the
//!   default), as for `work up`;
//! - the daemon's own environment and working directory, which the child
//!   inherited;
//! - the target's tmux server, which the child got as `TEND_TMUX_SOCKET`,
//!   here as the selected mux endpoint (`mux_control::with_endpoint`), so
//!   every tmux command and pane lookup goes to that one server;
//! - for a native session, the environment the child's `caller_env` read:
//!   the daemon's, `TEND_SOCKET` pinned, and the request's allowlisted keys;
//! - an IPC client on the socket the child had pinned as `TEND_SOCKET`;
//! - for every tmux process it starts, a closed stdin, `TEND_SOCKET` and the
//!   child's 30 s deadline (`tend_work::subprocess`);
//! - the agent-directory invalidation the CLI sent after every
//!   `agent start`.
//!
//! It runs on a blocking thread with its own single-threaded runtime, as
//! `work up` does. A request for an rmux server declines: the child got that
//! server as `$RMUX` with the pane variables cleared, which code outside the
//! mux endpoint reads too (backend detection, the caller's own pane). So
//! does an unparseable argv or unreadable config. A declined request runs
//! the child. `TEND_AGENT_START_CHILD=1` forces the child.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tend::agent_control::{
    AgentStartError, AgentStartRequest, AgentStartResult, InProcessAgentStart,
};

pub(crate) fn install(own_socket: PathBuf) {
    let run: InProcessAgentStart =
        Arc::new(move |request: AgentStartRequest, socket: Option<PathBuf>| {
            let socket = socket.unwrap_or_else(|| own_socket.clone());
            Box::pin(async move { run(request, socket).await })
        });
    tend::agent_control::install_in_process_agent_start(run);
}

async fn run(
    request: AgentStartRequest,
    socket: PathBuf,
) -> Option<Result<AgentStartResult, AgentStartError>> {
    if tend::agent_control::targets_rmux(&request) {
        return None;
    }
    let args = match tend_work::agent_launch::parse_start_argv(&request.arguments()) {
        Ok(args) => args,
        Err(error) => {
            tracing::warn!(%error, "in-process agent start cannot read its arguments");
            return None;
        }
    };
    let config_path = crate::work_up_in_process::cli_config_path(
        std::env::var_os("TEND_CONFIG"),
        tend::paths::default_config_file(),
    );
    let config = match tend::Config::load_or_default(config_path.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "in-process agent start cannot read the config");
            return None;
        }
    };
    // The child failed here too, before running anything.
    let endpoint = match tend::agent_control::tmux_socket_path(&request) {
        Ok(path) => path.map(|path| tend::BackendEndpoint {
            host: tend::HostKind::Tmux,
            socket: path.display().to_string(),
        }),
        Err(error) => return Some(Err(error)),
    };
    let env = native_env(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
        &socket,
        &request.env,
    );
    let work = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| AgentStartError::Failed(format!("agent start runtime: {error}")))?;
        let scope = tend_work::subprocess::Scope {
            socket: socket.clone(),
            deadline: std::time::Instant::now() + tend::work_control::WORK_COMMAND_TIMEOUT,
            cancel: None,
        };
        tend_work::subprocess::with_scope(scope, || {
            tend_work::mux_control::with_endpoint(endpoint, || {
                runtime.block_on(async move {
                    let client = tend::ipc::Client::new(socket.clone());
                    let result =
                        tend_work::agent_launch::launch_json(args, &client, &socket, &config, env)
                            .await;
                    // As the CLI did after every `agent start`, success or not.
                    let _ = client.invalidate_agent_directory().await;
                    let value = result.map_err(|error| {
                        AgentStartError::Failed(crate::work_up_in_process::last_line(&format!(
                            "{error:?}"
                        )))
                    })?;
                    serde_json::from_value::<AgentStartResult>(value)
                        .map_err(AgentStartError::InvalidJson)
                })
            })
        })
    });
    Some(match work.await {
        Ok(result) => result,
        Err(error) => Err(AgentStartError::Failed(format!(
            "agent start task: {error}"
        ))),
    })
}

/// The environment the child's `caller_env` handed a native session: its
/// own (the daemon's), with `TEND_SOCKET` set to the accepting daemon and the
/// request's allowlisted keys on top.
fn native_env(
    inherited: impl Iterator<Item = (String, String)>,
    socket: &Path,
    overrides: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited
        .filter(|(key, _)| key != "TEND_SOCKET" && !overrides.contains_key(key))
        .collect();
    env.push(("TEND_SOCKET".into(), socket.display().to_string()));
    env.extend(
        overrides
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_session_gets_the_env_the_child_passed_on() {
        let inherited = [
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin"),
            ("TEND_SOCKET", "/elsewhere.sock"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()));
        let overrides = BTreeMap::from([("PATH".to_string(), "/opt/bin:/usr/bin".to_string())]);
        let mut env = native_env(inherited, Path::new("/run/tendd.sock"), &overrides);
        env.sort();
        assert_eq!(
            env,
            [
                ("HOME", "/home/u"),
                ("PATH", "/opt/bin:/usr/bin"),
                ("TEND_SOCKET", "/run/tendd.sock"),
            ]
            .map(|(key, value)| (key.to_string(), value.to_string()))
        );
    }

    #[test]
    fn the_daemon_argv_parses_back_into_the_same_launch() {
        let request = AgentStartRequest {
            agent: "codex".into(),
            host: None,
            placement: tend::agent_control::AgentPlacement::Window,
            target: Some("%3".into()),
            tmux_socket: None,
            direction: None,
            cwd: PathBuf::from("/srv/app"),
            prompt: Some("--model is not a flag here".into()),
            name: Some("w".into()),
            role: Some("reviewer".into()),
            options: Some(vec!["--model".into(), "gpt-5-codex".into()]),
            env: BTreeMap::new(),
        };
        let args = tend_work::agent_launch::parse_start_argv(&request.arguments()).unwrap();
        assert!(args.json);
        assert_eq!(args.target.as_deref(), Some("%3"));
        assert_eq!(args.prompt.as_deref(), Some("--model is not a flag here"));
        assert_eq!(args.role.as_deref(), Some("reviewer"));
        assert_eq!(args.options, ["--model", "gpt-5-codex"]);
        assert_eq!(args.cwd, Some(PathBuf::from("/srv/app")));
    }
}
