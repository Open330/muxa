//! tmux CLI wrapper.
//!
//! Uses shell-outs for now. Control mode (`tmux -C`) will replace this once
//! we need real-time events (focus-changed, pane-close, etc.).
//!
//! The single-socket helpers in this module talk to whatever tmux server
//! `$TMUX_TMPDIR` / the `default` socket points to. For the global view —
//! every tmux server running for this user — see [`scanner`].

pub mod scanner;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Absolute path to the `tmux` binary, resolved once per process.
///
/// `tmux_command()` relies on `$PATH`, which is fine for an
/// interactive shell but fails inside `launchd`-spawned `muxad` whose
/// inherited `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin` — neither
/// Homebrew prefix is on that list. A failed shell-out collapsed to an
/// empty pane inventory drove the reconciler to reap every paned agent
/// each tick, wiping `last_prompt` on every row.
///
/// Resolution order:
/// 1. `$PATH` lookup via `tmux -V`. Cheap (~5 ms once) and the steady-state
///    path everywhere except launchd.
/// 2. Known Homebrew install prefixes — Apple Silicon then Intel.
/// 3. Bare `tmux` as a last-resort sentinel so the eventual error message
///    matches the prior behavior.
/// Build a `Command` for shelling out to tmux with the resolved binary
/// path and a UTF-8 locale pre-applied.
///
/// launchd's gui-domain user-agents inherit no `LANG`/`LC_*` env, leaving
/// child processes in the POSIX (C) locale. tmux in that locale silently
/// transliterates non-ASCII bytes — including the literal TAB characters
/// our format strings rely on as field separators — to `_`. The result
/// is a `list-panes` payload that exits 0 with 1.9 KB of stdout but parses
/// to zero rows, so the reconciler then reaps every paned agent.
///
/// Set `LC_ALL` (overrides every `LC_*`) to a UTF-8 locale on every tmux
/// invocation so the daemon and the user's interactive shell see byte-
/// identical output. `en_US.UTF-8` is universally available on macOS;
/// `C.UTF-8` would work on recent macOS too but is less portable.
pub fn tmux_command() -> Command {
    let mut cmd = Command::new(tmux_binary());
    cmd.env("LC_ALL", "en_US.UTF-8");
    cmd
}

pub fn tmux_binary() -> &'static Path {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        if Command::new("tmux")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return PathBuf::from("tmux");
        }
        ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux"]
            .iter()
            .find(|p| Path::new(p).exists())
            .map_or_else(|| PathBuf::from("tmux"), PathBuf::from)
    })
}

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("running tmux: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("tmux returned non-zero exit: {0}")]
    NonZero(String),
    #[error("unexpected tmux output: {0}")]
    BadOutput(String),
}

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub session: String,
    pub window_index: String,
    pub pane_index: String,
    pub tty: String,
    pub current_command: String,
    pub title: String,
    /// PID of the pane's initial process (typically the shell tmux spawned).
    /// `0` means "unknown" — backends that can't supply it (zellij CLI today,
    /// truncated lines from older tmux) leave it zeroed out, and downstream
    /// discovery treats `0` as "no process tree to walk."
    pub pane_pid: u32,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// tmux's stable session id (e.g. `$3`). Prefer this over the mutable
    /// session name for persisted counters.
    pub session_id: String,
    pub name: String,
    /// Number of attached clients. A session is "active" for muxa's
    /// cumulative timer when this is greater than zero.
    pub attached_clients: u32,
}

#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Name of the session currently displayed by this tmux client.
    pub session: String,
    /// tmux control-mode clients are automation, not an interactive user
    /// looking at a foreground session, so duration tracking ignores them.
    pub control_mode: bool,
}

/// `tmux -F` format string for `list-panes`. Tab-separated columns parsed
/// in `parse_pane_lines`. Kept `pub(crate)` so [`scanner`] can reuse it.
pub(crate) const PANE_FMT: &str =
    "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_tty}\t#{pane_current_command}\t#{pane_title}\t#{pane_pid}";

pub(crate) const SESSION_FMT: &str = "#{session_id}\t#{session_name}\t#{session_attached}";
pub(crate) const CLIENT_FMT: &str = "#{client_session}\t#{client_control_mode}";

/// Parse the `\t`-separated stdout of `tmux list-panes -F PANE_FMT` into
/// `PaneInfo` rows. Lines with too few columns are silently skipped — the
/// caller only sees well-formed rows. The `pane_pid` column was added in
/// 0.5.x; rows from older `PANE_FMT` outputs (or other backends that
/// don't emit it) get `pane_pid = 0`.
pub(crate) fn parse_pane_lines(stdout: &str) -> Vec<PaneInfo> {
    let mut panes = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }
        let pane_pid = cols.get(7).and_then(|s| s.parse().ok()).unwrap_or(0);
        panes.push(PaneInfo {
            pane_id: cols[0].into(),
            session: cols[1].into(),
            window_index: cols[2].into(),
            pane_index: cols[3].into(),
            tty: cols[4].into(),
            current_command: cols[5].into(),
            title: cols[6].into(),
            pane_pid,
        });
    }
    panes
}

pub(crate) fn parse_session_lines(stdout: &str) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 {
            continue;
        }
        let attached_clients = cols[2].trim().parse::<u32>().unwrap_or(0);
        sessions.push(SessionInfo {
            session_id: cols[0].into(),
            name: cols[1].into(),
            attached_clients,
        });
    }
    sessions
}

pub(crate) fn parse_client_lines(stdout: &str) -> Vec<ClientInfo> {
    let mut clients = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 || cols[0].is_empty() {
            continue;
        }
        clients.push(ClientInfo {
            session: cols[0].into(),
            control_mode: matches!(cols[1].trim(), "1" | "true"),
        });
    }
    clients
}

pub fn list_panes() -> Result<Vec<PaneInfo>, TmuxError> {
    // Under `launchd` (gui-domain user agent) tmux's default socket
    // lookup resolves to a different temp dir than the user's
    // interactive shell, so a bare `tmux list-panes -a` finds no server
    // and returns nothing. Enumerate every known socket (the same
    // dedup'd /tmp/tmux-<uid> + /private/tmp/tmux-<uid> set the scanner
    // uses) and aggregate; fall back to the bare call only when no
    // enumerable socket exists (e.g. CI sandboxes).
    let sockets = scanner::enumerate_sockets();
    if sockets.is_empty() {
        return list_panes_bare();
    }
    let mut all: Vec<PaneInfo> = Vec::new();
    let mut last_err: Option<TmuxError> = None;
    for sock in &sockets {
        let Some(sock_str) = sock.to_str() else {
            continue;
        };
        match tmux_command()
            .args(["-S", sock_str, "list-panes", "-a", "-F", PANE_FMT])
            .output()
        {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                all.extend(parse_pane_lines(&stdout));
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                // "no server running" is the steady-state for stale socket
                // files left behind by crashed servers; treat as empty
                // rather than a hard error.
                if !stderr.starts_with("no server running on") {
                    last_err = Some(TmuxError::NonZero(stderr));
                }
            }
            Err(e) => {
                last_err = Some(TmuxError::Spawn(e));
            }
        }
    }
    if all.is_empty() {
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    Ok(all)
}

fn list_panes_bare() -> Result<Vec<PaneInfo>, TmuxError> {
    let out = tmux_command()
        .args(["list-panes", "-a", "-F", PANE_FMT])
        .output()?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))?;
    Ok(parse_pane_lines(&stdout))
}

pub fn list_sessions() -> Result<Vec<SessionInfo>, TmuxError> {
    let out = tmux_command()
        .args(["list-sessions", "-F", SESSION_FMT])
        .output()?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))?;
    Ok(parse_session_lines(&stdout))
}

pub fn list_clients() -> Result<Vec<ClientInfo>, TmuxError> {
    let out = tmux_command()
        .args(["list-clients", "-F", CLIENT_FMT])
        .output()?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))?;
    Ok(parse_client_lines(&stdout))
}

pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Capture the visible contents of a tmux pane, preserving ANSI escape
/// sequences so the caller can render colors / attributes faithfully.
///
/// Wraps `tmux capture-pane -ep -t <pane_id>`:
///   - `-p` writes to stdout instead of a paste buffer.
///   - `-e` keeps escape sequences for color and attribute codes — without
///     this the output is plain text and we lose all color information.
///
/// Errors map onto `TmuxError`: spawn failure (no tmux at all), non-zero
/// exit (pane no longer exists, or no tmux server), or non-UTF-8 stdout.
/// The "pane gone" case is the most common in practice — tmux's exit
/// status is non-zero, the stderr is short — so callers should treat
/// `NonZero` as "ephemeral, retry next tick" rather than fatal.
pub fn capture_pane(pane_id: &str) -> Result<String, TmuxError> {
    let out = tmux_command()
        .args(["capture-pane", "-ep", "-t", pane_id])
        .output()?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))
}

/// Best-effort resolution of the user's active pane.
///
/// `$TMUX_PANE` covers the common case (a shell running inside that pane).
/// But tmux does NOT propagate it to processes spawned by `run-shell`, key
/// bindings, or `display-popup` — including the
/// `bind-key s display-popup -E "muxa watch"` recipe shipped in
/// `examples/muxa.tmux.conf`. In those contexts we ask tmux directly,
/// scoping the query to the session id parsed out of `$TMUX` so that with
/// multiple attached clients we still return the active pane of the
/// session that triggered the binding (rather than tmux's most-recently-
/// active client, which `display-message` defaults to).
pub fn current_pane() -> Option<String> {
    if let Some(p) = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty()) {
        return Some(p);
    }
    let target = parse_tmux_session_target(&std::env::var("TMUX").ok()?)?;
    let out = tmux_command()
        .args(["display-message", "-p", "-t", &target, "#{pane_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let pane = stdout.lines().next().unwrap_or("").trim();
    if pane.is_empty() {
        None
    } else {
        Some(pane.to_string())
    }
}

/// Parse a tmux target spec for the session this client is attached to,
/// out of the `$TMUX` env var. `$TMUX` is `socket_path,server_pid,session_id`
/// where `session_id` is numeric; tmux accepts `$<id>` as a session target.
///
/// Returns `None` when the env var is malformed or the trailing field isn't
/// a plain decimal id, so callers fall back to "no target known".
fn parse_tmux_session_target(tmux_env: &str) -> Option<String> {
    let sid = tmux_env.rsplit(',').next()?;
    if sid.is_empty() || !sid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("${sid}"))
}

/// Resolve a raw tmux `pane_id` (e.g. `%42`) to its full `PaneInfo`.
///
/// Returns `None` if tmux is unavailable, if the pane no longer exists, or
/// if `list-panes` fails for any other reason. Callers should be ready to
/// fall back to showing the raw id.
pub fn resolve_pane(pane_id: &str) -> Option<PaneInfo> {
    list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
}

/// Map of tmux pane shell PIDs → tmux pane id (e.g. `12345 -> "%42"`).
///
/// Used by the hook adapter's ancestry-walk fallback in
/// [`crate::adapters::hook::run_hook`]: when `TMUX_PANE` isn't in the
/// hook process's environment, walk parent PIDs and look them up here
/// to recover the pane the SDK-spawned agent actually belongs to.
///
/// Empty on any failure (tmux unavailable, server down, etc.). Callers
/// treat the empty map as "no fallback available" and proceed with
/// `pane: None`.
pub fn pane_pid_map() -> std::collections::HashMap<u32, String> {
    let out = match tmux_command()
        .args(["list-panes", "-a", "-F", "#{pane_pid}\t#{pane_id}"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return std::collections::HashMap::new(),
    };
    let Ok(stdout) = String::from_utf8(out) else {
        return std::collections::HashMap::new();
    };
    parse_pane_pid_map(&stdout)
}

/// Pulled out for direct unit testing — `pane_pid_map` itself shells
/// out to `tmux`.
pub(crate) fn parse_pane_pid_map(stdout: &str) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    for line in stdout.lines() {
        let mut cols = line.split('\t');
        let Some(pid_str) = cols.next() else { continue };
        let Some(pane_id) = cols.next() else { continue };
        let Ok(pid) = pid_str.trim().parse::<u32>() else {
            continue;
        };
        out.insert(pid, pane_id.into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pane_pid_map_well_formed() {
        let stdout = "12345\t%10\n67890\t%11\n";
        let map = parse_pane_pid_map(stdout);
        assert_eq!(map.get(&12345).map(String::as_str), Some("%10"));
        assert_eq!(map.get(&67890).map(String::as_str), Some("%11"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_pane_pid_map_skips_garbage_lines() {
        let stdout = "12345\t%10\nnot-a-pid\t%99\n67890\t%11\n\n";
        let map = parse_pane_pid_map(stdout);
        // The malformed pid line is dropped without aborting the parse.
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&12345));
        assert!(map.contains_key(&67890));
    }

    #[test]
    fn parses_existing_pane_lines_format() {
        // Sanity-check that the older `parse_pane_lines` still handles
        // the existing 7-column format — protects against accidental
        // breakage when adding the new pid map alongside it.
        let stdout = "%10\tmain\t0\t0\t/dev/pts/0\tclaude\tclaude session\n";
        let panes = parse_pane_lines(stdout);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "%10");
        assert_eq!(panes[0].current_command, "claude");
    }

    #[test]
    fn parses_session_lines() {
        let stdout = "$1\tmain\t1\n$2\twork\t0\n";
        let sessions = parse_session_lines(stdout);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "$1");
        assert_eq!(sessions[0].name, "main");
        assert_eq!(sessions[0].attached_clients, 1);
        assert_eq!(sessions[1].attached_clients, 0);
    }

    #[test]
    fn parses_client_lines() {
        let stdout = "main\t0\nwork\t1\n\t0\n";
        let clients = parse_client_lines(stdout);
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].session, "main");
        assert!(!clients[0].control_mode);
        assert_eq!(clients[1].session, "work");
        assert!(clients[1].control_mode);
    }

    #[test]
    fn parses_session_id_from_tmux_env() {
        assert_eq!(
            parse_tmux_session_target("/tmp/tmux-1044/default,82477,475"),
            Some("$475".into())
        );
    }

    #[test]
    fn rejects_malformed_tmux_env() {
        // Missing fields, non-numeric session id, empty string — all fall
        // through to None so the caller knows it can't scope the query.
        assert_eq!(parse_tmux_session_target(""), None);
        assert_eq!(parse_tmux_session_target("/tmp/sock,82477,abc"), None);
        assert_eq!(parse_tmux_session_target("/tmp/sock,82477,"), None);
    }
}
