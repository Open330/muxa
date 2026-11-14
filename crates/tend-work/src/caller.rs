//! What the calling process knows about itself: its own pane, whether that
//! pane env is borrowed from a shared Codex app-server, its collaboration
//! origin, and the environment a launched child inherits.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use tend::collaboration::CollaborationOrigin;

/// Whether this process inherited its pane env from a shared Codex
/// app-server. Ancestry is fixed for the life of the process, so walk once.
pub fn pane_env_is_borrowed() -> bool {
    static SHARED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHARED.get_or_init(|| {
        tend::adapters::proc_ancestry::under_shared_codex_app_server(std::process::id())
    })
}

/// This process's own `$TMUX_PANE`, or `None` when the pane env was inherited
/// from a shared Codex app-server and so names whichever pane started that
/// server. CLI defaults read the caller's pane through this so a command run
/// from a Codex thread never acts on (or places next to) another pane.
pub fn own_tmux_pane() -> Option<String> {
    own_pane_from(std::env::var("TMUX_PANE").ok(), pane_env_is_borrowed)
}

/// The backend's current pane (`$TMUX_PANE`, then `$TMUX`, `$ZELLIJ_PANE_ID`,
/// …), under the same rule as [`own_tmux_pane`]: `None` when the pane env was
/// inherited from a shared Codex app-server.
pub fn own_current_pane() -> Option<String> {
    own_pane_from(
        tend::identity_backend().current_pane(),
        pane_env_is_borrowed,
    )
}

pub fn own_pane_from(pane: Option<String>, borrowed: impl FnOnce() -> bool) -> Option<String> {
    let pane = pane.filter(|pane| !pane.trim().is_empty())?;
    (!borrowed()).then_some(pane)
}

pub fn current_collaboration_origin() -> std::result::Result<CollaborationOrigin, String> {
    let pane = tend::identity_backend()
        .current_pane()
        .or_else(current_process_host_pane)
        .ok_or_else(|| {
            "collaboration could not identify this MCP server's pane; \
             native pane variables are unset and process ancestry did not reach a pane shell. \
             For Codex, add env_vars = [\"RMUX\", \"RMUX_PANE\", \"TMUX\", \"TMUX_PANE\", \"TEND_SOCKET\"] \
             under [mcp_servers.tend] and restart Codex"
                .to_string()
        })?;
    let endpoint = match tend::backend::pane_id_host_kind(&pane) {
        Some(tend::HostKind::Cmux) => Some(tend::backend::cmux::endpoint_from_env()),
        Some(tend::HostKind::Rmux) => std::env::var("RMUX").ok(),
        Some(tend::HostKind::Tmux) => std::env::var("TMUX").ok(),
        Some(tend::HostKind::Zellij | tend::HostKind::Herdr) | None => None,
    };
    let socket = endpoint.and_then(|value| {
        let path = value.split(',').next()?.trim();
        (!path.is_empty()).then(|| tend::backend::pane_endpoint_identity(Some(&pane), path))
    });
    // An MCP caller *is* the agent in the pane, so it keeps the pane identity:
    // replies must route back to it and wake it.
    Ok(CollaborationOrigin {
        exact_session: false,
        pane,
        socket,
        console: false,
        agent_session_id: None,
    })
}

/// Recover the owning pane when an MCP host sanitizes subprocess
/// environment variables. Current Codex releases only forward variables
/// explicitly listed in `mcp_servers.<name>.env_vars`, so an existing tend
/// registration may launch without the host's native variables even though
/// Codex itself is running in a pane.
///
/// The MCP process is still a descendant of the pane shell. Match the first
/// ancestor whose PID appears in an active backend's pane inventory. Explicit
/// env remains authoritative because it also identifies the endpoint without
/// ambiguity.
pub fn current_process_host_pane() -> Option<String> {
    let pane_pids = tend::active_backends()
        .into_iter()
        .flat_map(|backend| backend.pane_pid_map())
        .collect::<HashMap<_, _>>();
    pane_from_ancestry(std::process::id(), &pane_pids, |pid| {
        tend::adapters::proc_ancestry::parent_pid(pid)
    })
}

pub fn caller_env(socket_path: &Path) -> Vec<(String, String)> {
    let mut env = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<Vec<_>>();
    match env.iter_mut().find(|(key, _)| key == "TEND_SOCKET") {
        Some((_, value)) if value.is_empty() => {
            *value = socket_path.display().to_string();
        }
        Some(_) => {}
        None => env.push(("TEND_SOCKET".into(), socket_path.display().to_string())),
    }
    env
}

#[allow(clippy::implicit_hasher)] // the pane map always comes from a backend inventory
pub fn pane_from_ancestry<F>(
    start_pid: u32,
    pane_pids: &HashMap<u32, String>,
    parent_of: F,
) -> Option<String>
where
    F: Fn(u32) -> Option<u32>,
{
    if let Some(pane) = pane_pids.get(&start_pid) {
        return Some(pane.clone());
    }
    let candidates: HashSet<u32> = pane_pids.keys().copied().collect();
    let pane_shell =
        tend::adapters::proc_ancestry::ancestor_in_set(start_pid, &candidates, parent_of)?;
    pane_pids.get(&pane_shell).cloned()
}
