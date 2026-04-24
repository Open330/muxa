//! tmux CLI wrapper.
//!
//! Uses shell-outs for now. Control mode (`tmux -C`) will replace this once
//! we need real-time events (focus-changed, pane-close, etc.).

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

const PANE_FMT: &str =
    "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_tty}\t#{pane_current_command}\t#{pane_title}";

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
    Ok(panes)
}

pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

pub fn current_pane() -> Option<String> {
    std::env::var("TMUX_PANE").ok()
}
