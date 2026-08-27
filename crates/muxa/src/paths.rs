//! XDG-aware default paths.

use std::path::PathBuf;

pub const SOCKET_FILENAME: &str = "muxa.sock";
pub const CONFIG_DIRNAME: &str = "muxa";
pub const CONFIG_FILENAME: &str = "config.toml";
pub const HISTORY_FILENAME: &str = "prompts.ndjson";
pub const ACTIVITY_FILENAME: &str = "activity.ndjson";
pub const STATE_FILENAME: &str = "state.json";
pub const SESSION_ACTIVITY_FILENAME: &str = "session-activity.json";
pub const COLLABORATION_FILENAME: &str = "collaboration.json";
pub const COLLABORATION_AUDIT_FILENAME: &str = "collaboration-audit.ndjson";
pub const ASK_FILENAME: &str = "ask.json";
pub const NODE_ID_FILENAME: &str = "host-id";
pub const DASHBOARD_WORK_FILENAME: &str = "dashboard-work.json";
pub const PIPELINE_RUN_FILENAME: &str = "pipeline-runs.json";
/// Subdirectory holding short-lived cross-process lock files. Its contents
/// carry no state — only the kernel's lock on an open descriptor — so it is
/// safe to delete wholesale when nothing is running.
pub const LOCK_DIRNAME: &str = "locks";

/// Default daemon socket path. Prefers `$XDG_RUNTIME_DIR/muxa.sock`; falls
/// back to `/tmp/muxa-<uid>.sock` when the runtime dir is unset.
pub fn default_socket() -> PathBuf {
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join(SOCKET_FILENAME);
    }
    PathBuf::from(format!("/tmp/muxa-{}.sock", posix_uid()))
}

/// Default config file path: `$XDG_CONFIG_HOME/muxa/config.toml`, falling
/// back to `$HOME/.config/muxa/config.toml`.
pub fn default_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(CONFIG_DIRNAME).join(CONFIG_FILENAME))
}

/// Default prompt-history file: `$XDG_DATA_HOME/muxa/prompts.ndjson`,
/// falling back to `$HOME/.local/share/muxa/prompts.ndjson`. Lives under
/// the data dir (not state/config) because prompts are user content the
/// operator may want to back up or grep through.
pub fn default_history_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(HISTORY_FILENAME))
}

/// Default activity ledger file: `$XDG_DATA_HOME/muxa/activity.ndjson`.
/// Stores closed duration intervals for agent states and tmux session
/// foreground time.
pub fn default_activity_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(ACTIVITY_FILENAME))
}

/// Default agent-registry snapshot file: `$XDG_DATA_HOME/muxa/state.json`,
/// falling back to `$HOME/.local/share/muxa/state.json`. Co-located with
/// the prompt history so a single backup or rotation policy covers both.
/// Lock file serializing handle allocation for one room, under
/// `$XDG_DATA_HOME/muxa/locks/`.
///
/// Claiming a handle is a read-modify-write over a namespace several
/// processes share, and tmux user options have no compare-and-set. Checking
/// again after the write cannot substitute: the pane that writes second can
/// finish its check before the first write lands, leaving neither willing to
/// yield. The exclusion has to come from outside tmux, so it comes from here.
///
/// Keyed by room — tmux socket plus window id — because that is the scope a
/// handle is unique within. `key` is expected pre-sanitized by the caller.
pub fn alias_lock_file(key: &str) -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(LOCK_DIRNAME).join(key))
}

pub fn default_state_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(STATE_FILENAME))
}

/// Stable physical-node identity used by Muxa Fleet. It intentionally lives
/// in the data directory rather than config: SSH aliases, host names, labels,
/// and even the config file can change without creating a different node.
pub fn default_node_id_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(NODE_ID_FILENAME))
}

/// Default tmux session activity file: `$XDG_DATA_HOME/muxa/session-activity.json`.
/// Co-located with the daemon's other user-state files so backups and
/// cleanup policies stay simple.
pub fn default_session_activity_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(SESSION_ACTIVITY_FILENAME))
}

/// Default durable collaboration mailbox snapshot.
/// Ask history: `$XDG_DATA_HOME/muxa/ask.json`, beside the
/// collaboration mailbox it mirrors.
pub fn default_ask_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(ASK_FILENAME))
}

pub fn default_collaboration_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(COLLABORATION_FILENAME))
}

/// Default append-only collaboration caller audit ledger.
pub fn default_collaboration_audit_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(COLLABORATION_AUDIT_FILENAME))
}

/// Durable logical Work records. Execution bindings remain owned by pane
/// backends; this file stores operator metadata and optional external issue
/// references keyed by `{workspace_id, work_id}`.
pub fn default_dashboard_work_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(DASHBOARD_WORK_FILENAME))
}

/// Durable desired graph and generation-aware state for Work pipeline Runs.
pub fn default_pipeline_run_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(CONFIG_DIRNAME).join(PIPELINE_RUN_FILENAME))
}

fn posix_uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
