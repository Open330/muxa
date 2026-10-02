//! Bind Codex threads that run behind the shared `codex app-server`.
//!
//! Codex 0.160+ runs every attached thread's hooks from one shared server, so
//! a thread's hook rows arrive without a pane. Nothing Codex sends maps a
//! thread to its terminal, so muxa binds it from evidence instead, strongest
//! first, and leaves it unbound whenever the evidence is ambiguous:
//!
//! 1. An explicit `codex resume <id>` process under exactly one pane.
//! 2. The one pane running an interactive Codex at the thread's cwd that no
//!    other thread already owns (`Store::correlate_paneless_codex_union`).
//! 3. When several such panes share that cwd, the pane whose screen shows the
//!    thread's latest prompt — and no other unbound thread's. This also covers
//!    `/new` inside a bound TUI: one TUI runs one thread at a time, so a
//!    *newer* thread whose prompt is on that pane takes it over and the
//!    previous thread is stopped (it has left every terminal).
//!
//! On demand the daemon tries 1, then 3, then 2; the reconciler's periodic tick
//! runs only the capture-free 1–2. The daemon runs the full pass
//! when a paneless Codex hook arrives and when an unbound thread makes its
//! first collaboration call, so threads started seconds apart in one cwd bind
//! one at a time instead of colliding.
//!
//! Known gap: a `/new` thread whose prompt is too short to be evidence can
//! still be handed an idle, never-prompted Codex pane at the same cwd by
//! elimination (2). Any prompt of 12+ characters is bound by its screen first.

use crate::backend::SharedBackend;
use crate::event::AgentKind;
use crate::state::{Agent, SharedStore};
use crate::tmux::PaneInfo;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shortest prompt (in non-whitespace characters) trusted as screen evidence.
/// Shorter prompts ("ok", "continue") are too likely to appear elsewhere.
const MIN_PROMPT_EVIDENCE_CHARS: usize = 12;
/// How much of the prompt's start is matched. The TUI wraps and may truncate a
/// long prompt, but its opening is rendered verbatim right after submission.
const PROMPT_NEEDLE_CHARS: usize = 48;
/// Scrollback lines captured above the visible screen for prompt evidence.
const SCREEN_HISTORY_LINES: usize = 500;

/// Whether `agent` is a real Codex thread still waiting for a pane.
pub(crate) fn is_unplaced_codex(agent: &Agent) -> bool {
    agent.kind == AgentKind::Codex
        && agent.state != crate::event::AgentState::Stopped
        && agent.pane.is_none()
        && agent.surface.is_none()
        && agent.pid.is_none()
        && !agent.session_id.starts_with("synthetic-")
}

/// Run every binding strategy once over `panes` (listed from `backends` when
/// `None`). Returns how many threads were bound or rebound.
pub(crate) async fn bind_paneless_codex(
    store: &SharedStore,
    backends: &[SharedBackend],
    panes: Option<Vec<PaneInfo>>,
) -> usize {
    let panes = if let Some(panes) = panes {
        panes
    } else {
        let listed = backends.to_vec();
        tokio::task::spawn_blocking(move || {
            listed
                .iter()
                .flat_map(|backend| backend.list_panes())
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default()
    };
    if panes.is_empty() {
        return 0;
    }
    let scanned = panes.clone();
    let scan = tokio::task::spawn_blocking(move || crate::process_tree::scan_codex_panes(&scanned))
        .await
        .unwrap_or_default();
    let mut bound = store.rebind_codex_resumes(&scan.resume_bindings).await;

    // Screen evidence before elimination: after `/new` the thread runs in a
    // pane its predecessor owns, and elimination would hand it any idle Codex
    // pane at the cwd instead. Only a cwd with an unbound thread is captured.
    let contested = store.contested_codex_hosts(&scan.hosts).await;
    if !contested.is_empty() {
        bound += prompt_pass(store, backends, contested).await;
    }
    bound
        + store
            .correlate_paneless_codex_union(&panes, &scan.hosts)
            .await
}

async fn prompt_pass(
    store: &SharedStore,
    backends: &[SharedBackend],
    contested: Vec<PaneInfo>,
) -> usize {
    let capturers = backends.to_vec();
    let screens = tokio::task::spawn_blocking(move || {
        contested
            .into_iter()
            .filter_map(|pane| {
                let host = crate::backend::pane_id_host_kind(&pane.pane_id)?;
                let backend = capturers.iter().find(|b| b.kind() == host)?;
                // Scrollback too: by a thread's first collaboration call its
                // prompt has often scrolled off the visible screen.
                let text = backend.capture_pane_history_on(
                    pane.socket.as_deref(),
                    &pane.pane_id,
                    SCREEN_HISTORY_LINES,
                )?;
                Some((pane, text))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    store.correlate_paneless_codex_by_prompt(&screens).await
}

/// Coalesces hook-triggered binding passes: a burst of hooks runs one pass,
/// plus one more if hooks arrived while it ran (their rows may postdate the
/// pass's snapshot).
pub(crate) fn schedule_bind_paneless_codex(store: SharedStore, backends: Vec<SharedBackend>) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    static AGAIN: AtomicBool = AtomicBool::new(false);
    AGAIN.store(true, Ordering::SeqCst);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        loop {
            while AGAIN.swap(false, Ordering::SeqCst) {
                bind_paneless_codex(&store, &backends, None).await;
            }
            RUNNING.store(false, Ordering::SeqCst);
            // A hook that arrived between the last pass and the release above
            // found RUNNING set and left; pick its request up here.
            if !AGAIN.load(Ordering::SeqCst) || RUNNING.swap(true, Ordering::SeqCst) {
                break;
            }
        }
    });
}

/// The start of `prompt` as screen evidence: whitespace removed (the TUI's
/// wrapping inserts newlines and indentation, even inside Korean words), or
/// `None` when it is too short to be distinctive.
pub(crate) fn prompt_needle(prompt: &str) -> Option<String> {
    let compact: String = prompt.chars().filter(|c| !c.is_whitespace()).collect();
    (compact.chars().count() >= MIN_PROMPT_EVIDENCE_CHARS)
        .then(|| compact.chars().take(PROMPT_NEEDLE_CHARS).collect())
}

/// Screen text normalized the same way as [`prompt_needle`], with terminal
/// escape sequences (captures keep colors, and the TUI may style part of a
/// prompt) removed so they cannot split the needle.
pub(crate) fn compact_screen(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                // CSI: parameters/intermediates, then one final byte @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \\).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// Pair unbound threads with contested panes whose screen shows exactly that
/// thread's prompt. A pane two threads' prompts both match, or a thread whose
/// prompt shows in two panes, stays unbound.
pub(crate) fn prompt_pairs(
    rows: &[(String, String, String)],
    screens: &[(PaneInfo, String)],
) -> Vec<(String, PaneInfo)> {
    let compact: Vec<_> = screens
        .iter()
        .map(|(pane, text)| (pane, compact_screen(text)))
        .collect();
    let mut pairs: Vec<(String, PaneInfo)> = Vec::new();
    for (session, cwd, needle) in rows {
        let hits: Vec<_> = compact
            .iter()
            .filter(|(pane, text)| pane.current_path.trim() == cwd && text.contains(needle))
            .collect();
        if let [(pane, _)] = hits.as_slice() {
            pairs.push((session.clone(), (*pane).clone()));
        }
    }
    let mut claims: HashMap<(String, Option<String>), usize> = HashMap::new();
    for (_, pane) in &pairs {
        *claims
            .entry((pane.pane_id.clone(), pane.socket.clone()))
            .or_default() += 1;
    }
    pairs.retain(|(_, pane)| claims[&(pane.pane_id.clone(), pane.socket.clone())] == 1);
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, cwd: &str) -> PaneInfo {
        let mut pane: PaneInfo = serde_json::from_value(serde_json::json!({
            "pane_id": id, "session": "muxa", "window_index": "1", "pane_index": "0",
            "tty": "", "current_command": "aas", "title": "", "pane_pid": 10,
            "socket": "default"
        }))
        .unwrap();
        pane.current_path = cwd.into();
        pane
    }

    #[test]
    fn needle_ignores_wrapping_and_rejects_short_prompts() {
        assert_eq!(prompt_needle("ok"), None);
        assert_eq!(prompt_needle("  continue  "), None);
        let needle =
            prompt_needle("배포 나가도 될지 linear 일감들과 PR 확인해서 검토해주세요").unwrap();
        let wrapped = "› 배포 나가도 될지 linear 일감들과 PR 확\n  인해서 검토해주세요\n• Working";
        assert!(compact_screen(wrapped).contains(&needle));
        // A capture keeps colors; a styled span inside the prompt still matches.
        let styled = "\u{1b}[1m›\u{1b}[0m 배포 나가도 될지 \u{1b}[36mlinear\u{1b}[39m 일감들과 PR \
                      확인해서 검토해주세요\u{1b}]0;title\u{7}";
        assert!(compact_screen(styled).contains(&needle));
    }

    #[test]
    fn prompt_pairs_bind_only_unique_matches() {
        let a = pane("rmux:%1", "/repo");
        let b = pane("rmux:%2", "/repo");
        let screens = vec![
            (
                a.clone(),
                "› Review the auth middleware for token leaks\n".to_string(),
            ),
            (
                b.clone(),
                "› Write migration tests for the billing schema\n".to_string(),
            ),
        ];
        let row = |sid: &str, prompt: &str| {
            (
                sid.to_string(),
                "/repo".to_string(),
                prompt_needle(prompt).unwrap(),
            )
        };
        let pairs = prompt_pairs(
            &[
                row("thread-a", "Review the auth middleware for token leaks"),
                row("thread-b", "Write migration tests for the billing schema"),
            ],
            &screens,
        );
        let mut got: Vec<_> = pairs
            .iter()
            .map(|(s, p)| (s.as_str(), p.pane_id.as_str()))
            .collect();
        got.sort_unstable();
        assert_eq!(got, [("thread-a", "rmux:%1"), ("thread-b", "rmux:%2")]);

        // The same prompt in both panes proves nothing.
        let same = vec![
            (a.clone(), "› Run the full test suite please\n".to_string()),
            (b, "› Run the full test suite please\n".to_string()),
        ];
        assert!(prompt_pairs(&[row("x", "Run the full test suite please")], &same).is_empty());

        // Two threads whose prompts both show in one pane claim nothing.
        let shared = vec![(
            a,
            "› Review the auth middleware for token leaks\n› Write migration tests for the billing schema"
                .to_string(),
        )];
        assert!(prompt_pairs(
            &[
                row("thread-a", "Review the auth middleware for token leaks"),
                row("thread-b", "Write migration tests for the billing schema"),
            ],
            &shared,
        )
        .is_empty());
    }
}
