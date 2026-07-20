//! tmux implementation of [`crate::backend::PaneBackend`].
//!
//! Thin delegating wrapper around the existing `crate::tmux::*` free
//! functions — moving the implementation to be a struct method invocation
//! rather than a bare call site is the entire point. The daemon
//! constructs one [`TmuxBackend`] at startup and threads it through to
//! every caller that previously imported `crate::tmux::list_panes` and
//! friends. Once every caller is migrated, the bare functions can be
//! either removed or made `pub(crate)` — for now they coexist so this
//! refactor lands as a strict superset of v0.2.0 behavior.

use std::collections::HashMap;

use crate::backend::{HostKind, PaneBackend, PaneObservation};
use crate::tmux::{self, PaneInfo};

/// Production tmux backend. Stateless — every method shells out fresh
/// because `tmux list-panes` is cheap (~few ms) and the daemon already
/// caches pane inventories at higher layers (e.g. the reconciler ticks
/// at 30 s, the dashboard `PaneCache` TTLs at 2 s).
///
/// Constructed once at daemon / CLI startup and held for the process
/// lifetime; cheap to clone if a future caller wants to fan one out
/// across tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct TmuxBackend;

impl TmuxBackend {
    pub const fn new() -> Self {
        Self
    }
}

impl PaneBackend for TmuxBackend {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        // `crate::tmux::list_panes` returns `Result<Vec<_>, TmuxError>`;
        // the trait contract is "best-effort, empty on failure" so we
        // collapse the error path here. Concrete `Err` cases (no tmux
        // server, command exit non-zero) are exactly the ones callers
        // already treated as "host is down, retry next tick."
        tmux::list_panes().unwrap_or_default()
    }

    fn observe_panes(&self) -> PaneObservation {
        tmux::observe_panes()
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        tmux::resolve_pane(pane_id)
    }

    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        tmux::capture_pane(pane_id).ok()
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        tmux::pane_pid_map()
    }

    fn current_pane(&self) -> Option<String> {
        tmux::current_pane()
    }

    fn focus_pane(&self, pane_id: &str) -> bool {
        // Just the focus step — `switch-client` / `attach-session` in
        // the CLI is a separate concern (it crosses the process
        // boundary and depends on whether the user is *inside* tmux
        // already). Best-effort: a non-zero exit (pane closed mid-call)
        // collapses to `false` so the caller can show a "couldn't
        // jump" hint instead of bubbling a TmuxError up.
        crate::tmux::tmux_command()
            .args(["select-pane", "-t", pane_id])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        // `send-keys -l` (literal) so prompt text can't be reinterpreted as
        // a tmux key; env-scoped default server. Best-effort: a non-zero exit
        // (pane gone) collapses to `false`, which the daemon surfaces as a
        // send failure.
        crate::tmux::send_text(pane_id, text)
    }

    fn send_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        // Control-plane injection pinned to the pane's recorded server via
        // `-S <socket>` — `%5` exists on every tmux server, so this targets
        // the right one. Handles leading-dash / trailing-`;` / multi-line text
        // (see `crate::tmux::send_text_on`).
        crate::tmux::send_text_on(socket, pane_id, text)
    }

    fn capture_pane_on(&self, socket: Option<&str>, pane_id: &str) -> Option<String> {
        // Control-plane capture pinned to the pane's recorded server, so a
        // shared pane id can't read the wrong screen. Best-effort → `None`.
        crate::tmux::capture_pane_on(socket, pane_id).ok()
    }

    // `caps()` uses the default impl from the trait — tmux supports
    // every method the trait exposes (including `send_text`), so spelling
    // out the table here would just be noise.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `kind()` is the source-of-truth for which backend a record went
    /// through. Locked down so a future copy/paste of this impl into a
    /// zellij sibling can't accidentally inherit `HostKind::Tmux`.
    #[test]
    fn tmux_backend_reports_tmux_kind() {
        let b = TmuxBackend::new();
        assert_eq!(b.kind(), HostKind::Tmux);
    }

    /// Trait-object dispatch compiles and works — locks in the
    /// `Send + Sync + 'static` bounds the daemon relies on for
    /// stashing the backend on background tasks.
    #[test]
    fn tmux_backend_is_object_safe() {
        let _b: Box<dyn PaneBackend> = Box::new(TmuxBackend::new());
    }
}
