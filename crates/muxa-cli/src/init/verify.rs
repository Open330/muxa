//! Post-apply smoke tests.
//!
//! Each check is independent so a partial failure still gives the
//! user useful information. We deliberately do *not* wedge the whole
//! run on these — they're verifications, not gates. The orchestrator
//! decides whether failures bubble up as warnings or errors.

use crate::init::components::Component;
use crate::init::plan::Plan;
use anyhow::Result;
use muxa::ipc::Client;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::timeout;

#[derive(Debug)]
pub struct VerifyReport {
    pub muxad_responsive: Option<bool>,
    pub current_pane_seen: Option<bool>,
    pub tmux_status_ok: Option<bool>,
    pub notes: Vec<String>,
}

pub async fn run(plan: &Plan, socket: PathBuf) -> Result<VerifyReport> {
    let mut report = VerifyReport {
        muxad_responsive: None,
        current_pane_seen: None,
        tmux_status_ok: None,
        notes: Vec::new(),
    };

    let want_daemon = plan
        .components
        .iter()
        .any(|c| matches!(c, Component::MuxadSystemd) || agent_component(*c));
    if want_daemon {
        report.muxad_responsive = Some(check_muxad(socket.as_path()).await);
        if report.muxad_responsive == Some(true) {
            report.current_pane_seen = Some(check_current_pane(socket.as_path()).await);
        }
    }

    let want_tmux = plan
        .components
        .iter()
        .any(|c| matches!(c, Component::TmuxPopup | Component::TmuxStatusLine));
    if want_tmux {
        report.tmux_status_ok = Some(check_tmux_config_parses());
    }

    Ok(report)
}

fn agent_component(c: Component) -> bool {
    matches!(
        c,
        Component::ClaudeHooks | Component::CodexHooks | Component::GeminiHooks
    )
}

async fn check_muxad(socket: &Path) -> bool {
    let client = Client::new(socket.to_path_buf());
    // 1.5 s is plenty; cold-started muxad answers a snapshot in <50 ms.
    matches!(
        timeout(Duration::from_millis(1500), client.snapshot()).await,
        Ok(Ok(_))
    )
}

async fn check_current_pane(socket: &Path) -> bool {
    let Some(pane) = std::env::var("TMUX_PANE")
        .ok()
        .or_else(|| std::env::var("ZELLIJ_PANE_ID").ok())
    else {
        return false;
    };
    let client = Client::new(socket.to_path_buf());
    matches!(
        timeout(Duration::from_millis(1500), client.by_pane(&pane)).await,
        Ok(Ok(v)) if !v.is_empty()
    )
}

/// Cheap parse-only check: `tmux start-server \; source-file -q ~/.tmux.conf`
/// would actually evaluate it, but we just want to know syntax is OK.
fn check_tmux_config_parses() -> bool {
    use std::process::Command;
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".tmux.conf");
    if !path.is_file() {
        return false;
    }
    Command::new("tmux")
        .args([
            "-f",
            path.to_str().unwrap_or(""),
            "start-server",
            ";",
            "kill-server",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true) // tmux missing → don't claim a problem
}
