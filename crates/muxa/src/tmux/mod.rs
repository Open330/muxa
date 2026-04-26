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

pub fn current_pane() -> Option<String> {
    std::env::var("TMUX_PANE").ok()
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
