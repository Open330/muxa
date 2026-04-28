//! Backfill discovery: scan host panes and synthesize `Started` events for
//! agent processes the daemon doesn't know about yet.
//!
//! When `muxad` restarts mid-session, agents that were already running keep
//! going inside their panes but stop appearing in `muxa status` until they
//! emit a fresh hook. That can take minutes if the agent is idle.
//!
//! This module bridges the gap by inspecting each pane's
//! `current_command` field via [`PaneBackend`] and synthesizing a minimal
//! `AgentEvent::Started` for any pane whose command matches a known
//! agent CLI. The synthetic entry uses a `synthetic-<pane_id>` session
//! id so re-running discovery is idempotent; when a real hook later
//! fires for the same pane, the store replaces the synthetic entry in
//! place — see `Store::apply`.
//!
//! Detection is intentionally simple: just `current_command`. We don't
//! walk process trees because that's noisy across kernels (`/proc` on
//! Linux, `ps` on macOS) and the foreground command is good enough at
//! the resolution we need.
//!
//! ## Capability gating
//!
//! Backends whose `caps().current_command == false` (zellij CLI without
//! the WASM plugin, today) cannot classify panes by command. Discovery
//! collapses to a no-op on those hosts rather than producing
//! all-`Unknown` rows: agent registration falls back to "trust whatever
//! pane the stdin hook self-reports" until the plugin lands.

use crate::backend::PaneBackend;
use crate::event::{AgentEvent, AgentId, AgentKind};
use crate::ipc::{Client, RuntimeError};
use crate::state::SYNTHETIC_SESSION_PREFIX;
use crate::tmux::PaneInfo;
use time::OffsetDateTime;

/// One discovered agent pane.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub pane: PaneInfo,
    pub kind: AgentKind,
}

/// Map a `pane_current_command` value to an `AgentKind`.
///
/// Returns `None` for commands that don't match any known agent. Matching is
/// case-insensitive on the command name only — argv tail isn't considered
/// because tmux only exposes the basename.
pub fn classify_command(cmd: &str) -> Option<AgentKind> {
    match cmd.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(AgentKind::ClaudeCode),
        "codex" => Some(AgentKind::Codex),
        "gemini" | "gemini-cli" => Some(AgentKind::GeminiCli),
        _ => None,
    }
}

/// Filter a list of panes down to the ones running a known agent CLI.
///
/// Pure function over a slice — easy to unit-test without touching tmux.
pub fn discover_from_panes(panes: &[PaneInfo]) -> Vec<Discovered> {
    panes
        .iter()
        .filter_map(|p| {
            classify_command(&p.current_command).map(|kind| Discovered {
                pane: p.clone(),
                kind,
            })
        })
        .collect()
}

/// Live scan: ask the backend for the current pane inventory and
/// classify each entry. Returns an empty vec when the backend has no
/// panes (host down) or when `caps().current_command == false` —
/// classification is meaningless without the foreground-command field.
pub fn scan_panes(backend: &dyn PaneBackend) -> Vec<Discovered> {
    if !backend.caps().current_command {
        return Vec::new();
    }
    discover_from_panes(&backend.list_panes())
}

/// Build a synthetic `AgentEvent::Started` for one discovered pane.
///
/// `cwd` is intentionally `None`: tmux doesn't expose the pane's working
/// directory in the format string, and process-tree probing is out of scope.
/// Real hook events will fill it in later.
pub fn synthesize_started(d: &Discovered, at: OffsetDateTime) -> AgentEvent {
    AgentEvent::Started {
        id: AgentId {
            kind: d.kind,
            session_id: format!("{}{}", SYNTHETIC_SESSION_PREFIX, d.pane.pane_id),
            pane: Some(d.pane.pane_id.clone()),
            cwd: None,
        },
        at,
    }
}

/// Outcome of one `run_discovery` pass — useful for logging and for the
/// `muxa sync` CLI summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscoveryReport {
    pub claude_code: usize,
    pub codex: usize,
    pub gemini_cli: usize,
    pub skipped_known: usize,
    pub failed: usize,
}

impl DiscoveryReport {
    pub fn total_ingested(&self) -> usize {
        self.claude_code + self.codex + self.gemini_cli
    }

    fn bump(&mut self, kind: AgentKind) {
        match kind {
            AgentKind::ClaudeCode => self.claude_code += 1,
            AgentKind::Codex => self.codex += 1,
            AgentKind::GeminiCli => self.gemini_cli += 1,
            // We never synthesize for Opencode/Unknown today.
            AgentKind::Opencode | AgentKind::Unknown => {}
        }
    }
}

/// Run a full discovery pass against the daemon.
///
/// Sequence:
///
/// 1. `tmux list-panes` and classify.
/// 2. Ask the daemon for a snapshot to skip already-known panes (cheap; the
///    store also has registry-level dedup, but filtering up front keeps the
///    summary line accurate and avoids no-op IPC chatter).
/// 3. Send a synthetic `Started` per remaining discovery via `Client::ingest`.
///
/// Errors from individual ingest calls are counted but don't abort the pass —
/// best-effort matches the rest of the daemon's surface.
pub async fn run_discovery(
    client: &Client,
    backend: &dyn PaneBackend,
) -> Result<DiscoveryReport, RuntimeError> {
    let discovered = scan_panes(backend);
    if discovered.is_empty() {
        // Either no panes (host down / outside multiplexer) or the
        // backend can't classify by command (zellij CLI-only). Skip
        // the IPC chatter — there's nothing to ingest.
        return Ok(DiscoveryReport::default());
    }

    let snapshot = client.snapshot().await?;
    let mut report = DiscoveryReport::default();
    let now = OffsetDateTime::now_utc();

    for d in discovered {
        // Skip panes that already have a non-stopped agent of the same kind.
        // The store's apply logic does the same check, but doing it here too
        // keeps the summary count honest.
        let already_known = snapshot.iter().any(|a| {
            a.kind == d.kind
                && a.pane.as_deref() == Some(d.pane.pane_id.as_str())
                && a.state != crate::AgentState::Stopped
        });
        if already_known {
            report.skipped_known += 1;
            continue;
        }

        let ev = synthesize_started(&d, now);
        match client.ingest(&ev).await {
            Ok(()) => report.bump(d.kind),
            Err(e) => {
                tracing::warn!(error = %e, pane = %d.pane.pane_id, "synthetic ingest failed");
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, cmd: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session: "s".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: "/dev/null".into(),
            current_command: cmd.into(),
            title: String::new(),
        }
    }

    #[test]
    fn classify_known_commands() {
        assert_eq!(classify_command("claude"), Some(AgentKind::ClaudeCode));
        assert_eq!(classify_command("codex"), Some(AgentKind::Codex));
        assert_eq!(classify_command("gemini"), Some(AgentKind::GeminiCli));
        assert_eq!(classify_command("gemini-cli"), Some(AgentKind::GeminiCli));
        // Case-insensitive — some shells uppercase argv[0].
        assert_eq!(classify_command("CLAUDE"), Some(AgentKind::ClaudeCode));
    }

    #[test]
    fn classify_drops_unknown() {
        assert_eq!(classify_command("zsh"), None);
        assert_eq!(classify_command("bash"), None);
        assert_eq!(classify_command(""), None);
        assert_eq!(classify_command("vim"), None);
    }

    #[test]
    fn discover_from_panes_filters_and_preserves_order() {
        let panes = vec![
            pane("%1", "zsh"),
            pane("%2", "claude"),
            pane("%3", "vim"),
            pane("%4", "codex"),
            pane("%5", "gemini-cli"),
            pane("%6", "less"),
        ];
        let out = discover_from_panes(&panes);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].pane.pane_id, "%2");
        assert_eq!(out[0].kind, AgentKind::ClaudeCode);
        assert_eq!(out[1].pane.pane_id, "%4");
        assert_eq!(out[1].kind, AgentKind::Codex);
        assert_eq!(out[2].pane.pane_id, "%5");
        assert_eq!(out[2].kind, AgentKind::GeminiCli);
    }

    #[test]
    fn synthesize_started_uses_stable_session_id() {
        let d = Discovered {
            pane: pane("%42", "claude"),
            kind: AgentKind::ClaudeCode,
        };
        let now = OffsetDateTime::now_utc();
        let ev = synthesize_started(&d, now);
        match ev {
            AgentEvent::Started { id, .. } => {
                assert_eq!(id.session_id, "synthetic-%42");
                assert_eq!(id.pane.as_deref(), Some("%42"));
                assert!(id.cwd.is_none());
                assert_eq!(id.kind, AgentKind::ClaudeCode);
            }
            other => panic!("expected Started, got {other:?}"),
        }
    }

    #[test]
    fn report_total_ingested_sums_kinds() {
        let mut r = DiscoveryReport::default();
        r.bump(AgentKind::ClaudeCode);
        r.bump(AgentKind::ClaudeCode);
        r.bump(AgentKind::Codex);
        r.bump(AgentKind::GeminiCli);
        // Bumps for kinds we don't synthesize must not move the total.
        r.bump(AgentKind::Opencode);
        r.bump(AgentKind::Unknown);
        assert_eq!(r.total_ingested(), 4);
        assert_eq!(r.claude_code, 2);
        assert_eq!(r.codex, 1);
        assert_eq!(r.gemini_cli, 1);
    }
}
