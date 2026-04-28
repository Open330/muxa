//! tmux CLI wrapper.
//!
//! Uses shell-outs for now. Control mode (`tmux -C`) will replace this once
//! we need real-time events (focus-changed, pane-close, etc.).
//!
//! The single-socket helpers in this module talk to whatever tmux server
//! `$TMUX_TMPDIR` / the `default` socket points to. For the global view —
//! every tmux server running for this user — see [`scanner`].

pub mod scanner;

use std::process::Command;

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
}

/// `tmux -F` format string for `list-panes`. Tab-separated columns parsed
/// in `parse_pane_lines`. Kept `pub(crate)` so [`scanner`] can reuse it.
pub(crate) const PANE_FMT: &str =
    "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_tty}\t#{pane_current_command}\t#{pane_title}";

/// Parse the `\t`-separated stdout of `tmux list-panes -F PANE_FMT` into
/// `PaneInfo` rows. Lines with too few columns are silently skipped — the
/// caller only sees well-formed rows.
pub(crate) fn parse_pane_lines(stdout: &str) -> Vec<PaneInfo> {
    let mut panes = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }
        panes.push(PaneInfo {
            pane_id: cols[0].into(),
            session: cols[1].into(),
            window_index: cols[2].into(),
            pane_index: cols[3].into(),
            tty: cols[4].into(),
            current_command: cols[5].into(),
            title: cols[6].into(),
        });
    }
    panes
}

pub fn list_panes() -> Result<Vec<PaneInfo>, TmuxError> {
    let out = Command::new("tmux")
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
    let out = Command::new("tmux")
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
    let out = Command::new("tmux")
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
    let out = match Command::new("tmux")
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
