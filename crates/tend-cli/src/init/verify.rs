//! Post-apply smoke tests.
//!
//! Each check is independent so a partial failure still gives the
//! user useful information. We deliberately do *not* wedge the whole
//! run on these — they're verifications, not gates. The orchestrator
//! decides whether failures bubble up as warnings or errors.

use crate::init::components::Component;
use crate::init::plan::Plan;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tend::ipc::Client;
use tokio::time::timeout;

#[derive(Debug)]
pub struct VerifyReport {
    pub tendd_responsive: Option<bool>,
    pub current_pane_seen: Option<bool>,
    pub notes: Vec<String>,
}

pub async fn run(plan: &Plan, socket: PathBuf) -> Result<VerifyReport> {
    let mut report = VerifyReport {
        tendd_responsive: None,
        current_pane_seen: None,
        notes: Vec::new(),
    };

    let want_daemon = plan
        .components
        .iter()
        .any(|c| matches!(c, Component::TenddSystemd) || agent_component(*c));
    if want_daemon {
        report.tendd_responsive = Some(check_tendd(socket.as_path()).await);
        if report.tendd_responsive == Some(true) {
            report.current_pane_seen = Some(check_current_pane(socket.as_path()).await);
        }
    }
    if plan.direction == super::plan::Direction::Install
        && plan
            .components
            .iter()
            .any(|c| super::integration::is_component(*c))
    {
        report.notes.push("Restart Codex / Claude Code to reload global instructions and MCP registrations. Run tend doctor to inspect links and registration drift.".into());
    }

    Ok(report)
}

fn agent_component(c: Component) -> bool {
    matches!(
        c,
        Component::ClaudeHooks
            | Component::CodexHooks
            | Component::AntigravityHooks
            | Component::OpencodeHooks
    )
}

async fn check_tendd(socket: &Path) -> bool {
    let client = Client::new(socket.to_path_buf());
    // 1.5 s is plenty; cold-started tendd answers a snapshot in <50 ms.
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

// NOTE — `check_tmux_config_parses()` was removed in v0.4.1 after a
// reported incident: the previous implementation shelled out to
// `tmux -f <conf> start-server \; kill-server` which kills the
// running tmux server on the *default* socket — i.e. the user's
// real sessions. The `-f` flag scopes the config but NOT the
// socket; isolating it would have required `-L <name>` / `-S <path>`.
//
// Given the catastrophic failure mode (silently wiping every session)
// vs. the modest upside (a syntax check after a write we already
// control end-to-end), we removed the check entirely instead of
// trying to harden it. Users see the diff in the review step before
// apply, our marker-block content is fixed, and `tmux source-file`
// will surface any error itself when it runs. The "tmux config
// syntax check" line in the final summary is gone too.
