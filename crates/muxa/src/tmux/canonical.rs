//! One row per physical pane.
//!
//! A session group (two terminals on one workspace: `callabo` plus client
//! sessions `callabo~view~N`) shares one window list, so `list-panes -a`
//! reports every pane once per member session. Those rows name the same pane
//! — same server, same pane id — and differ only in the session they were
//! listed under. A view row can also lack the workspace metadata, because
//! session options such as `@muxa_workspace_id` are not copied to the view.
//!
//! Anything that counts panes, requires "exactly one" match, keys work by
//! session, or does per-pane work (process scans, captures, IPC) must see
//! each pane once: duplicates make a unique pane look ambiguous, double a
//! count, or repeat side effects. Code that acts *for a particular client*
//! (switching that terminal's view) must keep the raw rows instead.
//!
//! The kept row is the group's canonical member — the oldest session, which
//! is the base the views were created from — the same member
//! `topology::fold_session_groups` folds onto, so every surface names a
//! grouped pane the same way.

use super::scanner::PaneSummary;
use super::PaneInfo;
use std::collections::HashMap;

/// A pane listing row that may repeat once per member of a session group.
pub trait GroupedPaneRow: Clone {
    /// Identity of the physical pane: server plus pane id.
    fn pane_key(&self) -> String;
    fn session_name(&self) -> &str;
    fn session_id(&self) -> &str;
    /// Fill workspace/work identity this row lacks from a duplicate.
    fn inherit_identity(&mut self, from: &Self);
}

/// The pane listing with each physical pane once (first-seen order),
/// represented by its group's canonical member. Workspace and work identity
/// missing from that row are taken from a duplicate that has them.
#[must_use]
pub fn canonical_panes<T: GroupedPaneRow>(panes: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(panes.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    for pane in panes {
        let key = pane.pane_key();
        let Some(&at) = index.get(&key) else {
            index.insert(key, out.len());
            out.push(pane.clone());
            continue;
        };
        let kept = &mut out[at];
        if prefer(pane, kept) {
            let mut replacement = pane.clone();
            replacement.inherit_identity(kept);
            *kept = replacement;
        } else {
            kept.inherit_identity(pane);
        }
    }
    out
}

impl GroupedPaneRow for PaneInfo {
    fn pane_key(&self) -> String {
        format!(
            "{}\u{0}{}",
            self.socket.as_deref().unwrap_or(""),
            self.pane_id
        )
    }
    fn session_name(&self) -> &str {
        &self.session
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn inherit_identity(&mut self, from: &Self) {
        if self.workspace_id.is_none() {
            self.workspace_id.clone_from(&from.workspace_id);
        }
        if self.work_id.is_none() {
            self.work_id.clone_from(&from.work_id);
        }
    }
}

impl GroupedPaneRow for PaneSummary {
    fn pane_key(&self) -> String {
        format!(
            "{}\u{0}{}\u{0}{}",
            self.host,
            self.socket.display(),
            self.pane_id
        )
    }
    fn session_name(&self) -> &str {
        &self.session
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn inherit_identity(&mut self, from: &Self) {
        let (into, from) = (&mut self.muxa, &from.muxa);
        if into.workspace_id.is_none() && from.workspace_id.is_some() {
            into.managed_workspace = from.managed_workspace;
            into.workspace_id.clone_from(&from.workspace_id);
            into.workspace_cwd.clone_from(&from.workspace_cwd);
        }
        if into.work_id.is_none() && from.work_id.is_some() {
            into.managed_work = from.managed_work;
            into.work_id.clone_from(&from.work_id);
            into.work_cwd.clone_from(&from.work_cwd);
        }
    }
}

/// `name` without a trailing `~view~N` client suffix: the session a grouped
/// view belongs to. Durable labels (activity log, history, stats, filters)
/// must use this so one workspace is not split across view names.
#[must_use]
pub fn base_session_name(mut name: &str) -> &str {
    while let Some((base, suffix)) = name.rsplit_once("~view~") {
        if base.is_empty() || suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        name = base;
    }
    name
}

/// Whether `candidate` should represent the pane instead of `kept` (two
/// listings of the same pane): the base session over a `~view~`, otherwise
/// the older session.
pub fn prefer<T: GroupedPaneRow>(candidate: &T, kept: &T) -> bool {
    let view = |p: &T| base_session_name(p.session_name()) != p.session_name();
    match (view(candidate), view(kept)) {
        (false, true) => true,
        (true, false) => false,
        _ => session_id_is_earlier(candidate.session_id(), kept.session_id()),
    }
}

/// tmux session ids are `$N`, allocated in creation order. Compare the
/// number, not the string (`$107` is newer than `$99`).
fn session_id_is_earlier(candidate: &str, kept: &str) -> bool {
    let number = |id: &str| id.strip_prefix('$').and_then(|n| n.parse::<u64>().ok());
    matches!((number(candidate), number(kept)), (Some(c), Some(k)) if c < k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session_id: &str, session: &str, pane_id: &str) -> PaneInfo {
        serde_json::from_value(serde_json::json!({
            "pane_id": pane_id, "session_id": session_id, "session": session,
            "session_group": "callabo", "window_id": "@2", "window_index": "2",
            "pane_index": "0", "tty": "", "current_command": "aas", "title": "",
            "pane_pid": 10, "socket": "/tmp/rmux-1044/default"
        }))
        .unwrap()
    }

    /// The live shape that broke Codex binding: `callabo` and two client
    /// views, where only the base carries the workspace stamp.
    #[test]
    fn grouped_rows_collapse_to_the_base_session() {
        let mut base = pane("$0", "callabo", "rmux:%2");
        base.workspace_id = Some("ws-callabo".into());
        let view_a = pane("$8", "callabo~view~743208", "rmux:%2");
        let view_b = pane("$9", "callabo~view~922474", "rmux:%2");
        let other = pane("$0", "callabo", "rmux:%24");

        for order in [
            vec![view_a.clone(), base.clone(), view_b.clone(), other.clone()],
            vec![base.clone(), view_a.clone(), view_b.clone(), other.clone()],
            vec![view_b.clone(), view_a.clone(), other.clone(), base.clone()],
        ] {
            let canonical = canonical_panes(&order);
            let ids: Vec<_> = canonical.iter().map(|p| p.pane_id.as_str()).collect();
            assert_eq!(ids.len(), 2, "{ids:?}");
            let p2 = canonical.iter().find(|p| p.pane_id == "rmux:%2").unwrap();
            assert_eq!(p2.session, "callabo");
            assert_eq!(p2.session_id, "$0");
            assert_eq!(p2.workspace_id.as_deref(), Some("ws-callabo"));
        }
    }

    #[test]
    fn a_surviving_view_inherits_identity_and_servers_stay_apart() {
        // Base closed: only views remain; the oldest one wins and keeps the
        // window-level work stamp either row carried.
        let mut a = pane("$9", "callabo~view~2", "rmux:%2");
        let mut b = pane("$8", "callabo~view~1", "rmux:%2");
        a.work_id = Some("work-7".into());
        b.workspace_id = Some("ws".into());
        let kept = canonical_panes(&[a, b]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session, "callabo~view~1");
        assert_eq!(kept[0].work_id.as_deref(), Some("work-7"));
        assert_eq!(kept[0].workspace_id.as_deref(), Some("ws"));

        // Same pane id on another server is another pane.
        let mut elsewhere = pane("$0", "callabo", "%2");
        elsewhere.socket = Some("default".into());
        let mut here = pane("$0", "callabo", "%2");
        here.socket = Some("other".into());
        assert_eq!(canonical_panes(&[elsewhere, here]).len(), 2);
    }

    #[test]
    fn base_names_strip_numeric_view_suffixes_only() {
        assert_eq!(base_session_name("callabo~view~743208"), "callabo");
        assert_eq!(base_session_name("a~view~1~view~2"), "a");
        assert_eq!(base_session_name("callabo"), "callabo");
        assert_eq!(base_session_name("x~view~beta"), "x~view~beta");
        assert_eq!(base_session_name("~view~3"), "~view~3");
    }
}
