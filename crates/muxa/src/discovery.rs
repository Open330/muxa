//! **Internal:** this module's items are `#[doc(hidden)]` — they're
//! `pub` for the workspace binaries (`muxad`) but excluded from
//! the public API surface and semver-exempt.
//!
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
//! Detection prefers the cheap `current_command` field. For common wrapper
//! foreground processes such as `node`, it does a bounded descendant lookup
//! so npm-shimmed CLIs still recover after daemon restarts.
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
#[cfg(not(target_os = "linux"))]
use crate::process_snapshot::ProcessTable;
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
    match command_name(cmd).to_ascii_lowercase().as_str() {
        "claude" => Some(AgentKind::ClaudeCode),
        "codex" => Some(AgentKind::Codex),
        "gemini" | "gemini-cli" => Some(AgentKind::GeminiCli),
        _ => None,
    }
}

fn command_name(cmd: &str) -> &str {
    cmd.trim().rsplit('/').next().unwrap_or(cmd).trim()
}

/// Foreground commands that commonly host an agent CLI as a child process
/// rather than running it directly. The npm-installed Codex CLI, for
/// example, lands in tmux as `node` (the JS shim) with the real
/// `codex` binary one or two `fork()`s deep — `classify_command` alone
/// misses that, so callers fall through to a process-tree walk.
fn is_wrapper_command(cmd: &str) -> bool {
    matches!(
        command_name(cmd).to_ascii_lowercase().as_str(),
        "node" | "deno" | "bun" | "python" | "python3" | "ruby"
    )
}

/// Filter a list of panes down to the ones running a known agent CLI.
///
/// Pure on `panes` for direct foreground commands. For wrapper panes it
/// reaches out to `/proc` on Linux, or takes one in-memory `ps` snapshot on
/// non-Linux and reuses it across every pane in this pass.
#[cfg(target_os = "linux")]
pub fn discover_from_panes(panes: &[PaneInfo]) -> Vec<Discovered> {
    panes
        .iter()
        .filter_map(|p| {
            classify_command(&p.current_command)
                .or_else(|| {
                    if is_wrapper_command(&p.current_command) {
                        classify_descendants(p.pane_pid)
                    } else {
                        None
                    }
                })
                .map(|kind| Discovered {
                    pane: p.clone(),
                    kind,
                })
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn discover_from_panes(panes: &[PaneInfo]) -> Vec<Discovered> {
    let needs_tree = panes.iter().any(|p| {
        classify_command(&p.current_command).is_none()
            && is_wrapper_command(&p.current_command)
            && p.pane_pid != 0
    });
    let table = needs_tree.then(crate::process_snapshot::read_current_process_table);

    panes
        .iter()
        .filter_map(|p| {
            classify_command(&p.current_command)
                .or_else(|| {
                    if is_wrapper_command(&p.current_command) {
                        table
                            .as_ref()
                            .and_then(|table| classify_descendants_in_table(p.pane_pid, table))
                    } else {
                        None
                    }
                })
                .map(|kind| Discovered {
                    pane: p.clone(),
                    kind,
                })
        })
        .collect()
}

/// Walk descendants of `pane_pid` looking for an agent binary. Returns
/// the first match (BFS-ish) within `MAX_TREE_DEPTH` levels. `pane_pid
/// == 0` means "backend didn't supply it" — bail without touching the
/// OS so non-Linux hosts that haven't filled in the field stay free.
pub fn classify_descendants(pane_pid: u32) -> Option<AgentKind> {
    if pane_pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        classify_descendants_with(pane_pid, MAX_TREE_DEPTH, &read_children, &read_comm)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let table = crate::process_snapshot::read_current_process_table();
        classify_descendants_in_table(pane_pid, &table)
    }
}

#[cfg(not(target_os = "linux"))]
fn classify_descendants_in_table(pane_pid: u32, table: &ProcessTable) -> Option<AgentKind> {
    classify_descendants_in_table_with_depth(pane_pid, MAX_TREE_DEPTH, table)
}

#[cfg(not(target_os = "linux"))]
fn classify_descendants_in_table_with_depth(
    root: u32,
    max_depth: u8,
    table: &ProcessTable,
) -> Option<AgentKind> {
    if max_depth == 0 {
        return None;
    }
    for child in table.children(root) {
        if let Some(name) = table.comm(*child) {
            if let Some(k) = classify_command(name) {
                return Some(k);
            }
        }
        if let Some(k) = classify_descendants_in_table_with_depth(*child, max_depth - 1, table) {
            return Some(k);
        }
    }
    None
}

/// Cap on how far we descend below the pane's initial process. The
/// observed `node`-wrapped Codex case is two levels deep (shell ->
/// node -> real codex), so a bound of 4 leaves headroom for one more
/// fork/exec without risking a runaway walk on pathological trees.
const MAX_TREE_DEPTH: u8 = 4;

/// Pure tree-walk so the recursion is unit-testable without touching
/// `/proc`. Production callers pass [`read_children`] and [`read_comm`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn classify_descendants_with(
    root: u32,
    max_depth: u8,
    children: &dyn Fn(u32) -> Vec<u32>,
    comm: &dyn Fn(u32) -> Option<String>,
) -> Option<AgentKind> {
    if max_depth == 0 {
        return None;
    }
    for child in children(root) {
        if let Some(name) = comm(child) {
            if let Some(k) = classify_command(&name) {
                return Some(k);
            }
        }
        if let Some(k) = classify_descendants_with(child, max_depth - 1, children, comm) {
            return Some(k);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_children(pid: u32) -> Vec<u32> {
    // `/proc/<pid>/task/<pid>/children` is space-separated PIDs on a single
    // line. Cheap (one open + small read), and stable across Linux 3.5+.
    let path = format!("/proc/{pid}/task/{pid}/children");
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

#[cfg(target_os = "linux")]
fn read_comm(pid: u32) -> Option<String> {
    let path = format!("/proc/{pid}/comm");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
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
            surface: None,
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
            // We never synthesize for Opencode/Unknown/Task today.
            AgentKind::Opencode | AgentKind::Unknown | AgentKind::Task => {}
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
    // `scan_panes` shells out to tmux and (on non-Linux) reads the whole
    // process table via `ps`; both block. Keep them off the async worker so a
    // slow host can't stall other daemon tasks each discovery tick — mirroring
    // the `spawn_blocking` treatment the reconciler already gives this work.
    // `block_in_place` (vs `spawn_blocking`) lets us keep the `&dyn` borrow;
    // both `muxad` and the CLI run on the multi-threaded runtime.
    let discovered = tokio::task::block_in_place(|| scan_panes(backend));
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
            pane_pid: 0,
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
        // macOS `ps -o comm=` can return the full executable path for a
        // descendant binary, while tmux returns a basename.
        assert_eq!(
            classify_command("/opt/homebrew/lib/node_modules/@openai/codex/bin/codex"),
            Some(AgentKind::Codex)
        );
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
    fn wrapper_command_covers_node_family() {
        assert!(is_wrapper_command("node"));
        assert!(is_wrapper_command("/opt/homebrew/bin/node"));
        assert!(is_wrapper_command("python3"));
        assert!(is_wrapper_command("deno"));
        assert!(is_wrapper_command("BUN")); // case-insensitive
                                            // Shells aren't wrappers — pane_current_command being a shell
                                            // usually means no agent is foregrounded, not that one is
                                            // hiding two levels down.
        assert!(!is_wrapper_command("zsh"));
        assert!(!is_wrapper_command("bash"));
        // Real agents stay out so the cheap top-level match wins first.
        assert!(!is_wrapper_command("claude"));
    }

    #[test]
    fn classify_descendants_finds_grandchild() {
        // Tree: root(node) -> 100(MainThread) -> 200(codex)
        // Mirrors the npm-installed Codex layout observed in the wild.
        let children = |pid: u32| match pid {
            1 => vec![100],
            100 => vec![200],
            _ => vec![],
        };
        let comm = |pid: u32| {
            Some(
                match pid {
                    100 => "MainThread",
                    200 => "codex",
                    _ => return None,
                }
                .to_string(),
            )
        };
        assert_eq!(
            classify_descendants_with(1, MAX_TREE_DEPTH, &children, &comm),
            Some(AgentKind::Codex),
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn classify_descendants_from_process_table_finds_full_path_agent() {
        use crate::process_snapshot::{ProcessInfo, ProcessTable};

        let table = ProcessTable::from_processes(vec![
            ProcessInfo {
                pid: 100,
                parent_pid: 1,
                depth: 0,
                comm: "MainThread".into(),
                cmdline: "node shim.js".into(),
            },
            ProcessInfo {
                pid: 200,
                parent_pid: 100,
                depth: 0,
                comm: "/opt/homebrew/lib/node_modules/@openai/codex/bin/codex".into(),
                cmdline: "codex".into(),
            },
        ]);

        assert_eq!(
            classify_descendants_in_table(1, &table),
            Some(AgentKind::Codex)
        );
    }

    #[test]
    fn classify_descendants_short_circuits_on_first_match() {
        // First child IS a known agent — no need to descend further.
        let children = |_| vec![10, 20];
        let comm = |pid: u32| {
            Some(
                match pid {
                    10 => "claude",
                    20 => "codex",
                    _ => return None,
                }
                .to_string(),
            )
        };
        assert_eq!(
            classify_descendants_with(1, MAX_TREE_DEPTH, &children, &comm),
            Some(AgentKind::ClaudeCode),
        );
    }

    #[test]
    fn classify_descendants_returns_none_when_no_agent() {
        // Wrapper sitting on its own with no agent below — common false
        // positive we don't want to upgrade into a Discovered row.
        let children = |pid: u32| if pid == 1 { vec![5] } else { vec![] };
        let comm = |pid: u32| {
            Some(
                match pid {
                    5 => "ts-node",
                    _ => return None,
                }
                .to_string(),
            )
        };
        assert!(classify_descendants_with(1, MAX_TREE_DEPTH, &children, &comm).is_none());
    }

    #[test]
    fn classify_descendants_respects_depth_limit() {
        // Linear chain longer than the cap: the agent at the tail must
        // NOT be found, otherwise a malicious or runaway tree could
        // stall sync.
        let children = |pid: u32| {
            if pid < 100 {
                vec![pid + 1]
            } else {
                vec![]
            }
        };
        let comm = |pid: u32| {
            if pid == 100 {
                Some("codex".to_string())
            } else {
                Some(format!("node-{pid}"))
            }
        };
        assert!(classify_descendants_with(1, 3, &children, &comm).is_none());
    }

    #[test]
    fn classify_descendants_skips_when_pid_zero() {
        // pane_pid == 0 is the "backend didn't supply it" sentinel —
        // we must not touch the OS in that case.
        assert!(classify_descendants(0).is_none());
    }

    #[test]
    fn discover_from_panes_uses_descendants_for_node_wrapper() {
        // We can't easily inject mock OS-readers through `discover_from_panes`,
        // so this test asserts the surface contract: a pane with `current_command
        // = "node"` and `pane_pid = 0` (= "no tree to walk") is NOT misclassified,
        // even though "node" is on the wrapper list. The positive grandchild
        // path is covered by `classify_descendants_finds_grandchild` above.
        let panes = vec![pane("%9", "node")];
        let out = discover_from_panes(&panes);
        assert!(
            out.is_empty(),
            "node with no walkable tree must not be classified"
        );
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
