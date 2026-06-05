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
//! | `focus_pane`      | watch attach action (`Enter` to jump)            |
//! | `kind`            | telemetry, log lines, debug                      |
//! | `caps`            | callers that need to know "method is plugin-only |
//! |                   | and would be a silent no-op" up front            |
//!
//! Backends that don't naturally support a method (e.g. zellij has no
//! multi-server enumeration concept) return an empty vec or `None`
//! rather than `Result::Err` — callers already treat host-down /
//! pane-gone as ephemeral. Where "transient empty" and "structurally
//! unsupported" matter (e.g. the hook adapter wants to log differently
//! when zellij CLI cannot populate `pane_pid_map`), call sites consult
//! [`BackendCaps`] returned by [`PaneBackend::caps`] before degrading.

pub mod tmux;
pub mod zellij;

use std::collections::HashMap;
use std::sync::Arc;

use crate::tmux::PaneInfo;

/// Shared, multi-thread-safe handle to a pane backend.
///
/// The daemon constructs one of these at startup based on
/// [`detect_host_env`] and threads it to every consumer that previously
/// imported `crate::tmux::*` directly — reconciler liveness, discovery,
/// the hook ancestry walker, the watch refresh task, the daemon's
/// `enrich_from_history` step. CLI commands either receive an `Arc`
/// from the caller or build a fresh one through [`default_backend`].
///
/// `Arc` rather than `Box` so background tokio tasks can each hold a
/// clone without contending on a lock; the trait is `Send + Sync` so
/// concurrent calls into the same backend are safe.
pub type SharedBackend = Arc<dyn PaneBackend>;

/// Build the backend that matches the current process environment.
///
/// Resolution mirrors [`detect_host_env`]: `MUXA_HOST` override first,
/// then `ZELLIJ`, then `TMUX`, then a default. When no host is
/// detectable we fall back to [`tmux::TmuxBackend`] — its methods
/// degrade gracefully on a host with no tmux server (`list_panes`
/// returns empty, `capture_pane` returns `None`) and most operators
/// running muxa outside a multiplexer at all are debugging the
/// daemon, not driving it. A future "noop" backend could replace
/// this fallback if that assumption stops holding.
pub fn default_backend() -> SharedBackend {
    match detect_host_env() {
        Some(HostKind::Zellij) => Arc::new(zellij::ZellijBackend::new()),
        _ => Arc::new(tmux::TmuxBackend::new()),
    }
}

/// Identity of the pane host. Surfaced through telemetry and log lines so
/// operators running both backends can tell which one a given event went
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum HostKind {
    Tmux,
    Zellij,
}

/// Static capability descriptor. Callers that *need to know* whether a
/// method is structurally available (vs transiently failing) consult
/// this before degrading. The fields name specific behaviors rather
/// than methods because some methods are partial — `list_panes`
/// always works on zellij CLI but its `current_command` field is
/// always empty without the WASM plugin.
///
/// All fields default to "supported" so the tmux backend (and any
/// fully-featured future backend) doesn't have to spell out the
/// capability table — only backends with gaps zero out the relevant
/// flag.
///
/// The four-bool shape trips clippy's `struct_excessive_bools` lint;
/// allowed here because each flag really is an independent capability
/// (no state-machine ordering between them) and a `bitflags!` macro
/// would be overkill at this scale. Add an enum if a fifth flag lands
/// — until then, named bools are the most grep-able shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCaps {
    /// Whether `list_panes()`'s `PaneInfo.current_command` is populated.
    /// Zellij CLI returns the field empty until the WASM plugin lands;
    /// discovery falls back to "trust the stdin hook" when this is
    /// false instead of trying to classify panes by command.
    pub current_command: bool,
    /// Whether `pane_pid_map()` returns real data. Hook ancestry only
    /// walks the parent-pid chain when this is true; otherwise it
    /// trusts the env (`TMUX_PANE` / `ZELLIJ_PANE_ID`) and gives up
    /// quietly if that's missing too.
    pub pane_pid_map: bool,
    /// Whether `capture_pane()` returns real screen contents. The
    /// `muxa watch` live preview falls back to the prompt/response
    /// view when this is false.
    pub capture_pane: bool,
    /// Whether `focus_pane()` actually moves the user's view.
    /// Backends that can't focus (e.g. a future read-only adapter)
    /// return false here and the watch picker hides the "Enter to
    /// jump" hint accordingly.
    pub focus_pane: bool,
}

impl Default for BackendCaps {
    fn default() -> Self {
        Self {
            current_command: true,
            pane_pid_map: true,
            capture_pane: true,
            focus_pane: true,
        }
    }
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

    /// Move the user's view to `pane_id` — `select-pane -t <id>` on
    /// tmux, `zellij action focus-pane-with-id <id>` on zellij. The
    /// trait method is only the *navigation* step; full attach
    /// semantics (tmux `switch-client` / `attach-session`) stay in the
    /// CLI because they cross the daemon/CLI process boundary in ways
    /// the backend can't see. Returns `true` when the focus call
    /// succeeded; `false` (best-effort) when the pane is gone or the
    /// host couldn't action the request.
    fn focus_pane(&self, pane_id: &str) -> bool;

    /// Static capability descriptor. Default impl returns "everything
    /// supported" because that's the tmux shape and most backends
    /// model their gaps as exceptions to that baseline.
    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }

    /// Ingest a wholesale pane snapshot pushed by an out-of-process
    /// source — today the zellij WASM plugin, which forwards zellij
    /// `PaneUpdate` events to the daemon over the `BackendPaneSnapshot`
    /// IPC command (see [`docs/ZELLIJ.md`](../../../../docs/ZELLIJ.md)
    /// Step 2). Default no-op: backends that enumerate panes themselves
    /// (tmux) ignore external pushes; the zellij backend overrides this
    /// to replace its cached snapshot.
    fn ingest_pane_snapshot(&self, _panes: Vec<PaneInfo>) {}
}

/// `Arc<dyn PaneBackend>` is itself a backend — every method
/// delegates to the inner trait object. Lets the daemon construct one
/// `Arc` at startup and hand `.clone()`s to the reconciler, the watch
/// refresh task, the IPC server, etc., without each consumer having
/// to learn a different signature. Also unblocks
/// [`crate::reconcile::LivenessSource`]'s blanket impl: an
/// `Arc<dyn PaneBackend>` flows through as a `LivenessSource`
/// transparently because it is a `PaneBackend`.
///
/// `?Sized` so the impl covers `Arc<dyn PaneBackend>` directly, not
/// just `Arc<ConcreteBackend>`. The default `caps()` impl is
/// overridden here so `Arc::caps()` reaches into the concrete
/// implementation rather than returning the trait default.
impl<T: PaneBackend + ?Sized> PaneBackend for Arc<T> {
    fn kind(&self) -> HostKind {
        (**self).kind()
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        (**self).list_panes()
    }
    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        (**self).resolve_pane(pane_id)
    }
    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        (**self).capture_pane(pane_id)
    }
    fn pane_pid_map(&self) -> HashMap<u32, String> {
        (**self).pane_pid_map()
    }
    fn current_pane(&self) -> Option<String> {
        (**self).current_pane()
    }
    fn focus_pane(&self, pane_id: &str) -> bool {
        (**self).focus_pane(pane_id)
    }
    fn caps(&self) -> BackendCaps {
        (**self).caps()
    }
    fn ingest_pane_snapshot(&self, panes: Vec<PaneInfo>) {
        (**self).ingest_pane_snapshot(panes);
    }
}

/// Inspect the environment for an active host.
///
/// Resolution order:
///
/// 1. **`MUXA_HOST`** — if set to `"tmux"` or `"zellij"` (case-insensitive),
///    that wins regardless of what `TMUX` / `ZELLIJ` look like. Provides
///    an unambiguous override for nested-multiplexer setups (e.g. zellij
///    inside tmux, or `tmux new-session` from inside zellij) where
///    auto-detect can't tell which host the current shell really lives in.
///    Other values are ignored (treated as unset) so a typo doesn't pin
///    the daemon to the wrong host silently.
/// 2. **`ZELLIJ`** set → [`HostKind::Zellij`].
/// 3. **`TMUX`** set → [`HostKind::Tmux`].
///
/// When both `TMUX` and `ZELLIJ` are present, zellij wins by default.
/// The env doesn't carry "which one was set last" ordering, so this is
/// a pragmatic pick rather than a principled one — `MUXA_HOST` is the
/// escape hatch for the case where the auto-detect picks wrong.
///
/// Returns `None` outside both hosts; callers fall through to a no-op
/// backend in that case.
pub fn detect_host_env() -> Option<HostKind> {
    detect_from(|name| std::env::var(name).ok())
}

/// Decoupled-from-process-env variant of [`detect_host_env`] for tests.
/// `read("VAR")` returns the env var's value if set, else `None`.
/// Production calls go through [`detect_host_env`] which inspects the
/// real process environment; tests pass a closure so we don't mutate
/// `std::env` (forbidden by the workspace's `forbid(unsafe_code)`
/// posture).
fn detect_from(read: impl Fn(&str) -> Option<String>) -> Option<HostKind> {
    // 1. `MUXA_HOST` override — explicit operator intent always wins.
    if let Some(raw) = read("MUXA_HOST") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tmux" => return Some(HostKind::Tmux),
            "zellij" => return Some(HostKind::Zellij),
            // Empty / unknown / typo → fall through to auto-detect.
            // Logging the bad value here would be noisy at startup;
            // the daemon traces the resolved host kind on the first
            // backend call, which is enough to diagnose mismatches.
            _ => {}
        }
    }

    // 2. & 3. Auto-detect from host-set env vars.
    if read("ZELLIJ").is_some() {
        return Some(HostKind::Zellij);
    }
    if read("TMUX").is_some() {
        return Some(HostKind::Tmux);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that builds a `read` closure from a static lookup table.
    /// Tests pass `&[("TMUX", "1"), ...]`; missing keys read as
    /// `None`. Keeps the call sites short.
    fn env_reader(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// All host env vars unset → `None`. The daemon uses this signal to
    /// fall back to a no-op backend rather than spamming `tmux` calls
    /// on a host without a multiplexer running.
    #[test]
    fn detect_returns_none_when_no_host_env_set() {
        assert!(detect_from(env_reader(&[])).is_none());
    }

    /// Only `TMUX` set → tmux. Only `ZELLIJ` set → zellij. Locks the
    /// happy-path mapping for both backends.
    #[test]
    fn detect_picks_tmux_or_zellij_from_env() {
        assert_eq!(
            detect_from(env_reader(&[("TMUX", "1")])),
            Some(HostKind::Tmux),
        );
        assert_eq!(
            detect_from(env_reader(&[("ZELLIJ", "1")])),
            Some(HostKind::Zellij),
        );
    }

    /// When both env vars are present (nested multiplexers — rare but
    /// possible), zellij wins by default. The `MUXA_HOST` override
    /// covers the case where this default picks wrong.
    #[test]
    fn detect_prefers_zellij_when_both_env_vars_set() {
        assert_eq!(
            detect_from(env_reader(&[("TMUX", "1"), ("ZELLIJ", "1")])),
            Some(HostKind::Zellij),
        );
    }

    /// `MUXA_HOST=tmux` wins over `ZELLIJ` being set — the escape hatch
    /// for the "I'm in tmux even though my env still carries the parent
    /// zellij's vars" case. Mirror test for `MUXA_HOST=zellij`.
    #[test]
    fn detect_muxa_host_overrides_auto_detect() {
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "tmux"), ("ZELLIJ", "1")])),
            Some(HostKind::Tmux),
            "MUXA_HOST=tmux must beat a present ZELLIJ env var",
        );
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "zellij"), ("TMUX", "1")])),
            Some(HostKind::Zellij),
            "MUXA_HOST=zellij must beat a present TMUX env var",
        );
    }

    /// Override is case-insensitive and tolerates whitespace — users
    /// hand-type these in shell rcfiles where `MUXA_HOST=Zellij`,
    /// `MUXA_HOST=" tmux "`, etc., are realistic.
    #[test]
    fn detect_muxa_host_is_case_and_whitespace_tolerant() {
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "TMUX")])),
            Some(HostKind::Tmux),
        );
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", " Zellij ")])),
            Some(HostKind::Zellij),
        );
    }

    /// Unknown / empty `MUXA_HOST` falls through to auto-detect rather
    /// than pinning the daemon to a "no host" decision. Typos surface
    /// as auto-detect outcomes — consistent with the daemon picking
    /// up *some* working host rather than refusing to run.
    #[test]
    fn detect_unknown_muxa_host_falls_through_to_auto() {
        // Unknown value, no host env → None (no host running).
        assert!(detect_from(env_reader(&[("MUXA_HOST", "screen")])).is_none());
        // Unknown value, but TMUX is up → tmux wins via auto-detect.
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "screen"), ("TMUX", "1")])),
            Some(HostKind::Tmux),
        );
        // Empty MUXA_HOST is ignored too.
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", ""), ("ZELLIJ", "1")])),
            Some(HostKind::Zellij),
        );
    }

    /// `HostKind` round-trips through its `Display` impl as a stable
    /// lowercase string — the format telemetry pipelines depend on.
    #[test]
    fn host_kind_display_is_lowercase_stable() {
        assert_eq!(HostKind::Tmux.to_string(), "tmux");
        assert_eq!(HostKind::Zellij.to_string(), "zellij");
    }

    /// A minimal fake the rest of the test module reuses — keeps
    /// the boilerplate of a full `PaneBackend` impl out of every
    /// individual test body.
    struct FakeBackend {
        panes: Vec<PaneInfo>,
        caps: BackendCaps,
    }

    impl FakeBackend {
        fn with_panes(panes: Vec<PaneInfo>) -> Self {
            Self {
                panes,
                caps: BackendCaps::default(),
            }
        }
    }

    impl PaneBackend for FakeBackend {
        fn kind(&self) -> HostKind {
            HostKind::Zellij
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }
        fn resolve_pane(&self, id: &str) -> Option<PaneInfo> {
            self.panes.iter().find(|p| p.pane_id == id).cloned()
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            Some("hello\n".into())
        }
        fn pane_pid_map(&self) -> HashMap<u32, String> {
            self.panes
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        u32::try_from(100 + i).unwrap_or(u32::MAX),
                        p.pane_id.clone(),
                    )
                })
                .collect()
        }
        fn current_pane(&self) -> Option<String> {
            self.panes.first().map(|p| p.pane_id.clone())
        }
        fn focus_pane(&self, _: &str) -> bool {
            true
        }
        fn caps(&self) -> BackendCaps {
            self.caps
        }
    }

    fn fake_pane(id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session: "z".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
        }
    }

    /// Trait surface contract: every method on a hand-rolled fake
    /// behaves as documented. Adding a new required method to
    /// `PaneBackend` breaks this test loudly.
    #[test]
    fn fake_backend_satisfies_trait_contract() {
        let b: Box<dyn PaneBackend> = Box::new(FakeBackend::with_panes(vec![fake_pane("zj-1")]));
        assert_eq!(b.kind(), HostKind::Zellij);
        assert_eq!(b.list_panes().len(), 1);
        assert_eq!(b.resolve_pane("zj-1").unwrap().pane_id, "zj-1");
        assert!(b.resolve_pane("nope").is_none());
        assert_eq!(b.capture_pane("zj-1").as_deref(), Some("hello\n"));
        assert_eq!(b.pane_pid_map().get(&100).map(String::as_str), Some("zj-1"));
        assert_eq!(b.current_pane().as_deref(), Some("zj-1"));
        assert!(b.focus_pane("zj-1"));
        assert_eq!(b.caps(), BackendCaps::default());
    }

    /// End-to-end through the reconciler: a `PaneBackend` plugged into
    /// `Reconciler::new` drives stale-pane reaping exactly the way the
    /// daemon expects. Locks the bridge between the two abstractions
    /// so a future change to either side surfaces here instead of in
    /// production.
    #[tokio::test]
    async fn pane_backend_drives_reconciler_via_blanket_liveness_impl() {
        use crate::event::{AgentEvent, AgentId, AgentKind};
        use crate::reconcile::Reconciler;
        use crate::state::Store;
        use std::time::Duration;
        use time::macros::datetime;

        let store = Store::shared();
        let t0 = datetime!(2026-04-28 12:00:00 UTC);
        for sid in ["alive", "ghost"] {
            store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        kind: AgentKind::ClaudeCode,
                        session_id: sid.into(),
                        pane: Some(format!("%{sid}")),
                        cwd: None,
                    },
                    at: t0,
                })
                .await;
        }
        // Backend reports only %alive as live; %ghost should be reaped.
        let backend = FakeBackend::with_panes(vec![fake_pane("%alive")]);
        let r = Reconciler::new(store.clone(), backend, Duration::from_millis(10));
        let report = r.reconcile_once().await;
        assert_eq!(report.stale_panes_reaped, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "alive");
    }

    /// `caps()` defaults to "everything supported" so backends with
    /// gaps must opt out explicitly. Locks down the default-impl
    /// behavior so future fields land with backwards-compatible
    /// semantics.
    #[test]
    fn backend_caps_default_is_all_true() {
        let caps = BackendCaps::default();
        assert!(caps.current_command);
        assert!(caps.pane_pid_map);
        assert!(caps.capture_pane);
        assert!(caps.focus_pane);
    }
}
