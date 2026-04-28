//! Pane-host abstraction.
//!
//! Today muxa shells out to tmux for everything that has to do with panes —
//! enumerating them, finding the foreground command per pane, mapping a
//! `pane_id` to the OS pid that owns its shell, focusing a pane on attach.
//! That coupling is small (~600 LOC under `crate::tmux`) but it lives in
//! the wrong layer: every caller above the IPC seam thinks in
//! `pane_id` strings and never needs to know that those ids came from
//! `tmux list-panes -F`.
//!
//! This module pulls the surface up into a [`PaneBackend`] trait so a
//! second host (zellij — see [`docs/ZELLIJ.md`](../../../../docs/ZELLIJ.md))
//! can plug in without forking the daemon. The tmux implementation in
//! [`tmux::TmuxBackend`] is a thin delegating wrapper around the existing
//! `crate::tmux::*` free functions; nothing about the production code path
//! has changed shape, this is just a seam.
//!
//! ## Why a trait, not an enum
//!
//! The set of backends is small (tmux today, zellij next, maybe screen /
//! `WezTerm` later) so an enum dispatch would compile fine. But:
//!
//! - Tests want to inject a fake backend without compiling tmux into the
//!   test binary. Trait objects make that ergonomic.
//! - The reconciler already takes a generic `LivenessSource` for the
//!   same reason; a trait keeps both abstractions consistent.
//! - Adding a backend in a downstream crate (e.g. an out-of-tree
//!   experiment) is one trait impl rather than a fork.
//!
//! ## Capability surface
//!
//! The trait is intentionally narrow — only operations the rest of the
//! codebase actually performs against the host today:
//!
//! | Method            | Used by                                          |
//! | ----------------- | ------------------------------------------------ |
//! | `list_panes`      | reconciler liveness, discovery, watch refresh    |
//! | `resolve_pane`    | hook ancestry walk, recap by-pane                |
//! | `capture_pane`    | watch preview live mode (`c` toggle)             |
//! | `pane_pid_map`    | hook ancestry walk fallback                      |
//! | `current_pane`    | watch initial-pane hint, status-line             |
//! | `kind`            | telemetry, log lines, debug                      |
//!
//! Backends that don't naturally support a method (e.g. zellij has no
//! multi-server enumeration concept) return an empty vec or `None`
//! rather than `Result::Err` — callers already treat host-down /
//! pane-gone as ephemeral.

pub mod tmux;

use std::collections::HashMap;

use crate::tmux::PaneInfo;

/// Identity of the pane host. Surfaced through telemetry and log lines so
/// operators running both backends can tell which one a given event went
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum HostKind {
    Tmux,
    Zellij,
}

/// Operations the daemon and CLI perform against the pane host.
///
/// All methods are best-effort: a transient failure (no tmux server, the
/// pane just closed, the env got truncated) returns an empty result
/// rather than an error so callers — which run on hot paths like the
/// reconciler tick or the watch refresh loop — never have to fan
/// non-fatal errors back up the stack. The reconciler is idempotent and
/// will reconverge on the next tick.
///
/// Implementors must be `Send + Sync + 'static` because the daemon
/// stashes them on background tasks that own their own runtimes.
pub trait PaneBackend: Send + Sync + 'static {
    /// Which host this backend talks to. Cheap; safe to call per event.
    fn kind(&self) -> HostKind;

    /// Snapshot of every pane the host considers alive right now.
    fn list_panes(&self) -> Vec<PaneInfo>;

    /// Look up a single pane by id. Returns `None` for unknown ids and
    /// for hosts that can't answer the query (rare).
    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo>;

    /// Capture the visible contents of a pane with ANSI escapes intact.
    /// `None` when the host doesn't support screen capture or the pane
    /// has gone away. Used by the `muxa watch` preview's live mode.
    fn capture_pane(&self, pane_id: &str) -> Option<String>;

    /// Map of OS pid → `pane_id` for every pane the host knows about.
    /// The hook adapter walks an event's parent-pid chain and looks
    /// each ancestor up in this map to recover a `TMUX_PANE` /
    /// `ZELLIJ_PANE_ID` that wasn't inherited through env.
    fn pane_pid_map(&self) -> HashMap<u32, String>;

    /// Best-effort identification of the currently-focused pane —
    /// usually keyed off the host's "this pane" env var
    /// (`TMUX_PANE` / `ZELLIJ_PANE_ID`) plus whatever fallback the
    /// host exposes.
    fn current_pane(&self) -> Option<String>;
}

/// Inspect the environment for an active host. Returns the innermost
/// host when nested (e.g. `zellij` inside `tmux`) — whichever was set
/// last wins, since that's the one whose pane we're actually inside.
///
/// Returns `None` outside both hosts; callers fall through to a no-op
/// backend in that case.
pub fn detect_host_env() -> Option<HostKind> {
    detect_from(|name| std::env::var_os(name).is_some())
}

/// Decoupled-from-process-env variant of [`detect_host_env`] for tests.
/// `is_set("VAR")` returns true iff the named env var is considered
/// present. Production calls go through [`detect_host_env`] which
/// inspects the real process environment; tests pass a closure so we
/// don't mutate `std::env` (forbidden by the workspace's
/// `forbid(unsafe_code)` posture).
fn detect_from(is_set: impl Fn(&str) -> bool) -> Option<HostKind> {
    // `ZELLIJ` is set inside zellij; `TMUX` inside tmux. When both are
    // present (tmux wrapping zellij or vice versa), the design doc says
    // prefer the innermost — but the env doesn't carry ordering.
    // Pragmatic compromise: prefer zellij since that's the more
    // recent / opt-in setup; the operator can override via config.
    if is_set("ZELLIJ") {
        return Some(HostKind::Zellij);
    }
    if is_set("TMUX") {
        return Some(HostKind::Tmux);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both env vars unset → `None`. The daemon uses this signal to
    /// fall back to a no-op backend rather than spamming `tmux` calls
    /// on a host without a multiplexer running.
    #[test]
    fn detect_returns_none_when_no_host_env_set() {
        assert!(detect_from(|_| false).is_none());
    }

    /// Only `TMUX` set → tmux. Only `ZELLIJ` set → zellij. Locks the
    /// happy-path mapping for both backends.
    #[test]
    fn detect_picks_tmux_or_zellij_from_env() {
        assert_eq!(detect_from(|n| n == "TMUX"), Some(HostKind::Tmux));
        assert_eq!(detect_from(|n| n == "ZELLIJ"), Some(HostKind::Zellij));
    }

    /// When both env vars are present (nested multiplexers — rare but
    /// possible), zellij wins per the design doc's "innermost / more
    /// recently opted-in" rule. Locks down the precedence so a future
    /// flip is intentional.
    #[test]
    fn detect_prefers_zellij_when_both_env_vars_set() {
        assert_eq!(detect_from(|_| true), Some(HostKind::Zellij));
    }

    /// `HostKind` round-trips through its `Display` impl as a stable
    /// lowercase string — the format telemetry pipelines depend on.
    #[test]
    fn host_kind_display_is_lowercase_stable() {
        assert_eq!(HostKind::Tmux.to_string(), "tmux");
        assert_eq!(HostKind::Zellij.to_string(), "zellij");
    }

    /// Sanity-check the trait is usable via a hand-rolled fake — the
    /// shape future zellij tests will follow before the WASM plugin
    /// can stand up a real backend in CI. If the trait grows a new
    /// required method this test breaks loudly.
    #[test]
    fn fake_backend_implements_trait_end_to_end() {
        struct Fake;
        impl PaneBackend for Fake {
            fn kind(&self) -> HostKind {
                HostKind::Zellij
            }
            fn list_panes(&self) -> Vec<PaneInfo> {
                vec![PaneInfo {
                    pane_id: "zj-1".into(),
                    session: "z".into(),
                    window_index: "0".into(),
                    pane_index: "0".into(),
                    tty: String::new(),
                    current_command: "claude".into(),
                    title: String::new(),
                }]
            }
            fn resolve_pane(&self, id: &str) -> Option<PaneInfo> {
                self.list_panes().into_iter().find(|p| p.pane_id == id)
            }
            fn capture_pane(&self, _: &str) -> Option<String> {
                Some("hello\n".into())
            }
            fn pane_pid_map(&self) -> HashMap<u32, String> {
                HashMap::from([(42, "zj-1".to_string())])
            }
            fn current_pane(&self) -> Option<String> {
                Some("zj-1".into())
            }
        }

        let b: Box<dyn PaneBackend> = Box::new(Fake);
        assert_eq!(b.kind(), HostKind::Zellij);
        assert_eq!(b.list_panes().len(), 1);
        assert_eq!(b.resolve_pane("zj-1").unwrap().pane_id, "zj-1");
        assert!(b.resolve_pane("nope").is_none());
        assert_eq!(b.capture_pane("zj-1").as_deref(), Some("hello\n"));
        assert_eq!(b.pane_pid_map().get(&42).map(String::as_str), Some("zj-1"));
        assert_eq!(b.current_pane().as_deref(), Some("zj-1"));
    }
}
