//! XDG-aware default paths.

use std::path::PathBuf;

pub const SOCKET_FILENAME: &str = "tend.sock";
pub const CONFIG_DIRNAME: &str = "tend";
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
pub const SNAPSHOT_DIRNAME: &str = "snapshots";
pub const PIPELINE_RUN_FILENAME: &str = "pipeline-runs.json";
pub const BACKUPS_DIRNAME: &str = "backups";

/// Whether `path` is absolute where it will be used. On this host that is
/// this platform's rule. A path for a request another Fleet host runs is
/// that host's, whose platform this one cannot assume: it must be absolute in
/// Unix form (`/srv/x`) or Windows form (`C:\x`, `\\server\share`), and that
/// host checks it again by its own rule when it runs the request.
#[must_use]
pub fn absolute_where_used(path: &std::path::Path, on_another_host: bool) -> bool {
    if path.is_absolute() {
        return true;
    }
    if !on_another_host {
        return false;
    }
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    text.starts_with('/')
        || text.starts_with(r"\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// Whether a request's Fleet `host` alias names another host (absent or
/// `local` is this one).
#[must_use]
pub fn is_remote_host(host: Option<&str>) -> bool {
    host.is_some_and(|host| host.trim() != "local")
}

/// [`std::fs::canonicalize`] in the form the rest of the system writes paths.
///
/// Windows canonicalizes to a verbatim path (`\\?\F:\work`), which no shell,
/// hook payload or multiplexer reports. Paths are compared as strings — an
/// agent's cwd against a pane's — so the prefix is dropped where the plain
/// form means the same thing.
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    Ok(without_verbatim_prefix(resolved))
}

fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// Default daemon socket path. Prefers `$XDG_RUNTIME_DIR/tend.sock`; falls
/// back to `/tmp/tend-<uid>.sock` when the runtime dir is unset.
pub fn default_socket() -> PathBuf {
    if let Some(dir) = xdg_dir("XDG_RUNTIME_DIR", dirs::runtime_dir) {
        return dir.join(SOCKET_FILENAME);
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/tmp/tend-{}.sock", posix_uid()))
    }
    // No `$XDG_RUNTIME_DIR` and no `/tmp` worth writing to: fall back to the
    // per-user local data directory. On Windows the path is not bound to a
    // filesystem object anyway — the transport derives a pipe name from it —
    // but it is still what `create_dir_all` runs against, and creating a
    // `\tmp` at the drive root would be a surprising thing for a daemon to do.
    #[cfg(not(unix))]
    {
        dirs::data_local_dir().map_or_else(
            || PathBuf::from(SOCKET_FILENAME),
            |d| d.join(CONFIG_DIRNAME).join(SOCKET_FILENAME),
        )
    }
}

/// Default config file path: `$XDG_CONFIG_HOME/tend/config.toml`, falling
/// back to `$HOME/.config/tend/config.toml`.
pub fn default_config_file() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", dirs::config_dir)
        .map(|d| d.join(CONFIG_DIRNAME).join(CONFIG_FILENAME))
}

/// Default prompt-history file: `$XDG_DATA_HOME/tend/prompts.ndjson`,
/// falling back to `$HOME/.local/share/tend/prompts.ndjson`. Lives under
/// the data dir (not state/config) because prompts are user content the
/// operator may want to back up or grep through.
pub fn default_history_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(HISTORY_FILENAME))
}

/// Default activity ledger file: `$XDG_DATA_HOME/tend/activity.ndjson`.
/// Stores closed duration intervals for agent states and tmux session
/// foreground time.
pub fn default_activity_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(ACTIVITY_FILENAME))
}

/// Default agent-registry snapshot file: `$XDG_DATA_HOME/tend/state.json`,
/// falling back to `$HOME/.local/share/tend/state.json`. Co-located with
/// the prompt history so a single backup or rotation policy covers both.
pub fn default_state_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(STATE_FILENAME))
}

/// Default directory for workspace snapshots:
/// `$XDG_DATA_HOME/tend/snapshots`. A snapshot is user-recoverable content —
/// the shape of a workspace plus what each pane was running — so it belongs
/// beside the prompt history rather than in the runtime or config dirs.
pub fn default_snapshot_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(SNAPSHOT_DIRNAME))
}

/// Stable physical-node identity used by Tend Fleet. It intentionally lives
/// in the data directory rather than config: SSH aliases, host names, labels,
/// and even the config file can change without creating a different node.
pub fn default_node_id_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(NODE_ID_FILENAME))
}

/// Default tmux session activity file: `$XDG_DATA_HOME/tend/session-activity.json`.
/// Co-located with the daemon's other user-state files so backups and
/// cleanup policies stay simple.
pub fn default_session_activity_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir)
        .map(|d| d.join(CONFIG_DIRNAME).join(SESSION_ACTIVITY_FILENAME))
}

/// Default durable collaboration mailbox snapshot.
/// Ask history: `$XDG_DATA_HOME/tend/ask.json`, beside the
/// collaboration mailbox it mirrors.
pub fn default_ask_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(ASK_FILENAME))
}

pub fn default_collaboration_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir)
        .map(|d| d.join(CONFIG_DIRNAME).join(COLLABORATION_FILENAME))
}

/// Default append-only collaboration caller audit ledger.
pub fn default_collaboration_audit_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir)
        .map(|d| d.join(CONFIG_DIRNAME).join(COLLABORATION_AUDIT_FILENAME))
}

/// Install and upgrade backups: `$XDG_DATA_HOME/tend/backups`. Each entry
/// is one backup (a directory or a file); `[storage].backups_keep` bounds how
/// many are kept.
pub fn default_backups_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir).map(|d| d.join(CONFIG_DIRNAME).join(BACKUPS_DIRNAME))
}

/// Durable logical Work records. Execution bindings remain owned by pane
/// backends; this file stores operator metadata and optional external issue
/// references keyed by `{workspace_id, work_id}`.
pub fn default_dashboard_work_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir)
        .map(|d| d.join(CONFIG_DIRNAME).join(DASHBOARD_WORK_FILENAME))
}

/// Durable desired graph and generation-aware state for Work pipeline Runs.
pub fn default_pipeline_run_file() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir)
        .map(|d| d.join(CONFIG_DIRNAME).join(PIPELINE_RUN_FILENAME))
}

#[cfg(unix)]
fn posix_uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

// dirs follows macOS conventions and ignores XDG overrides there. Honor the
// explicit overrides on every platform so isolated runtimes cannot accidentally
// share the user's state, history, or configuration.
/// The data directory: `$XDG_DATA_HOME` when it is set to an absolute path,
/// else the platform's own. `dirs::data_dir` reads `XDG_DATA_HOME` only on
/// Linux, so a sandboxed daemon on Windows or macOS would otherwise share
/// the user's real stores.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", dirs::data_dir)
}

fn xdg_dir(key: &str, fallback: fn() -> Option<PathBuf>) -> Option<PathBuf> {
    absolute_override(std::env::var_os(key)).or_else(fallback)
}

fn absolute_override(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_hosts_path_is_absolute_in_either_platforms_form() {
        use std::path::Path;
        for path in ["/srv/x", r"C:\x", "C:/x", r"\\server\share"] {
            assert!(absolute_where_used(Path::new(path), true), "{path}");
        }
        for path in ["srv", r"C:x", r"..\x", ""] {
            assert!(!absolute_where_used(Path::new(path), true), "{path}");
        }
        // Here, this platform's own rule.
        assert_eq!(
            absolute_where_used(Path::new("/srv/x"), false),
            Path::new("/srv/x").is_absolute()
        );
        assert!(!absolute_where_used(Path::new("srv"), false));
        assert!(is_remote_host(Some("dev")));
        assert!(!is_remote_host(Some("local")));
        assert!(!is_remote_host(None));
    }

    #[test]
    fn verbatim_windows_paths_come_back_plain() {
        let plain = |raw: &str| without_verbatim_prefix(PathBuf::from(raw));
        assert_eq!(
            plain(r"\\?\F:\workspace\tend"),
            PathBuf::from(r"F:\workspace\tend")
        );
        assert_eq!(
            plain(r"\\?\UNC\server\share\x"),
            PathBuf::from(r"\\server\share\x")
        );
        // A verbatim path with no plain spelling is left alone, as is any
        // path that never had the prefix.
        assert_eq!(
            plain(r"\\?\Volume{abc}\x"),
            PathBuf::from(r"\\?\Volume{abc}\x")
        );
        assert_eq!(plain("/home/june/work"), PathBuf::from("/home/june/work"));
    }

    #[test]
    fn explicit_xdg_paths_support_sandbox_isolation_on_every_platform() {
        let data = crate::test_paths::abs("/tmp/tend-test/data");
        assert_eq!(
            absolute_override(Some(data.clone().into())),
            Some(PathBuf::from(data))
        );
        assert_eq!(absolute_override(Some("relative/data".into())), None);
        assert_eq!(absolute_override(Some("".into())), None);
        assert_eq!(absolute_override(None), None);
    }
}
