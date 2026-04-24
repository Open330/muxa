//! Thin wrapper around `tmux` CLI for pane discovery.
//!
//! MVP uses shell-outs. Control mode (`tmux -C`) will replace this once we
//! need real-time events (focus, pane close, etc.).

use anyhow::{Context, Result};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,         // e.g. "%12"
    pub session: String,
    pub window_index: String,
    pub pane_index: String,
    pub tty: String,
    pub current_command: String,
    pub title: String,
}

pub fn list_panes() -> Result<Vec<PaneInfo>> {
    let fmt = "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_tty}\t#{pane_current_command}\t#{pane_title}";
    let out = Command::new("tmux")
        .args(["list-panes", "-a", "-F", fmt])
        .output()
        .context("running tmux list-panes")?;

    if !out.status.success() {
        anyhow::bail!(
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8(out.stdout)?;
    let mut panes = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }
        panes.push(PaneInfo {
            pane_id: cols[0].to_string(),
            session: cols[1].to_string(),
            window_index: cols[2].to_string(),
            pane_index: cols[3].to_string(),
            tty: cols[4].to_string(),
            current_command: cols[5].to_string(),
            title: cols[6].to_string(),
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
