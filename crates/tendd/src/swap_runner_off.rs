//! Stands in for `swap_runner.rs` in a build without the `aas` feature: no
//! swap service ever opens, so [`DaemonSwap`] cannot be made and every path
//! that would run a swap reports the module missing instead.

use super::{Arc, CollaborationStore, Submissions, SwapEnded, SwapStart};

/// Never constructed: there is no swap service in this build.
pub(crate) struct DaemonSwap {
    pub(crate) node_id: String,
    pub(crate) submissions: Arc<Submissions>,
    never: std::convert::Infallible,
}

#[allow(clippy::unused_async)] // the same shape as the real one
pub(crate) async fn drain(swap: &DaemonSwap, _timeout: std::time::Duration) -> bool {
    match swap.never {}
}

pub(crate) fn request_key(
    _node_id: &str,
    _pane: &str,
    _socket: Option<&str>,
    _agent_session_id: &str,
    _rule: &str,
    _episode: &str,
) -> String {
    String::new()
}

/// No job store in this build.
pub(crate) const JOB_SCHEMA: i64 = 0;

#[allow(clippy::unused_async)] // the same shape as the real one
pub(crate) async fn open_swap_service(
    _store: tend::SharedStore,
    _backends: Vec<tend::SharedBackend>,
    _swap: tend::swap_api::SwapConfig,
    _accounts: tend::accounts::AccountsConfig,
    _config_path: Option<std::path::PathBuf>,
    _collaboration: Arc<CollaborationStore>,
) -> Result<Arc<DaemonSwap>, String> {
    Err(tend::swap_api::module_unavailable(false))
}

pub(crate) async fn run_swap_operation(
    swap: Arc<DaemonSwap>,
    _key: String,
    _start: Option<SwapStart>,
) -> SwapEnded {
    match swap.never {}
}

pub(crate) fn jobs(swap: &Arc<DaemonSwap>) -> Arc<dyn tend::swap_api::SwapJobs> {
    match swap.never {}
}
