//! zellij implementation of [`crate::backend::PaneBackend`].
//!
//! ## What works without the WASM plugin
//!
//! zellij's CLI surface is narrower than tmux's `list-panes -F`. From
//! shell-outs alone we can:
//!
//! - **Focus a pane** via `zellij action focus-pane-with-id <id>` —
//!   stable, non-disruptive once the user knows the id.
//! - **Read the current pane** from the `$ZELLIJ_PANE_ID` env var
//!   inherited by every shell zellij spawns inside a pane.
//!
//! The other trait methods (`list_panes`, `resolve_pane`,
//! `capture_pane`, `pane_pid_map`) need pane-level metadata zellij
//! only exposes through the plugin API. This module pre-positions a
//! snapshot cache the future plugin will push into; until that lands,
//! the cache is permanently empty and the methods report it that way.
//! [`crate::backend::BackendCaps`] flags are flipped to `false` for the
//! plugin-only methods so callers (discovery, hook ancestry, watch
//! preview) can branch on capability rather than mistaking "structurally
//! unsupported" for a transient empty result.
//!
//! ## What about `capture_pane` via `dump-screen`?
//!
//! `zellij action dump-screen FILE` works, but it dumps the
//! **currently-focused** pane only — capturing a non-focused pane
//! requires `focus-pane-with-id` first, which is visibly disruptive
//! to the user (their view jumps away). For "live preview" UX that's
//! the wrong trade — a side-by-side preview should never disrupt the
//! source pane. We therefore advertise `capture_pane: false` in
//! `caps()` until the plugin can deliver non-disruptive captures via
//! `PaneUpdate`/`screenshot()` events.
//!
//! ## Threading model
//!
//! `ZellijBackend` is `Send + Sync` because the daemon stashes one in
//! an `Arc<dyn PaneBackend>` and shares it across background tokio
//! tasks. The cached snapshot lives behind an `RwLock` so plugin
//! pushes (writers) and consumers (readers) compose without blocking.
//! All read sites take a single `read()` and clone what they need —
//! holding the lock across an `await` would invite deadlocks against
//! the plugin's IPC writer task.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::backend::{BackendCaps, HostKind, PaneBackend};
use crate::tmux::PaneInfo;

/// CLI-baseline zellij backend.
///
/// Holds a shared snapshot of pane metadata that the future
/// `muxa-zellij-plugin` WASM module will populate over IPC. Without the
/// plugin the snapshot stays empty and methods that depend on
/// pane-level metadata return empty / `None` — `caps()` is the
/// authoritative signal for what's actually available.
///
/// Cheap to construct (no I/O); cheap to clone (`Arc` + `RwLock`).
#[derive(Debug, Default, Clone)]
pub struct ZellijBackend {
    snapshot: Arc<RwLock<ZellijSnapshot>>,
}

#[derive(Debug, Clone)]
struct ZellijSnapshot {
    panes: Vec<PaneInfo>,
    updated_at: Option<Instant>,
    ttl: Duration,
}

impl Default for ZellijSnapshot {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            updated_at: None,
            ttl: Duration::from_secs(10),
        }
    }
}

impl ZellijSnapshot {
    fn fresh_panes(&self) -> Vec<PaneInfo> {
        let Some(updated_at) = self.updated_at else {
            return Vec::new();
        };
        if updated_at.elapsed() > self.ttl {
            return Vec::new();
        }
        self.panes.clone()
    }
}

impl ZellijBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the cached pane snapshot wholesale. Reached via
    /// [`PaneBackend::ingest_pane_snapshot`] when the daemon's
    /// `BackendPaneSnapshot` IPC command delivers a fresh push from the
    /// WASM plugin. Atomic relative to readers (the `RwLock` write
    /// guard blocks new readers; in-flight readers see the previous
    /// snapshot).
    pub(crate) fn replace_snapshot(&self, panes: Vec<PaneInfo>) {
        if let Ok(mut w) = self.snapshot.write() {
            w.panes = panes;
            w.updated_at = Some(Instant::now());
        }
    }
}

impl PaneBackend for ZellijBackend {
    fn kind(&self) -> HostKind {
        HostKind::Zellij
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        // RwLock poisoning collapses to "empty list" rather than
        // panicking on a hot path; the daemon already treats
        // host-down as ephemeral.
        self.snapshot
            .read()
            .map(|s| s.fresh_panes())
            .unwrap_or_default()
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.snapshot
            .read()
            .ok()
            .and_then(|s| s.fresh_panes().into_iter().find(|p| p.pane_id == pane_id))
    }

    fn capture_pane(&self, _pane_id: &str) -> Option<String> {
        // See module docs: zellij's `dump-screen` only works on the
        // currently-focused pane, so capturing a *specific* pane via
        // CLI requires a disruptive focus jump. The watch live
        // preview opts out via `caps().capture_pane == false`.
        None
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        self.snapshot
            .read()
            .map(|s| {
                s.fresh_panes()
                    .into_iter()
                    .filter(|p| p.pane_pid != 0)
                    .map(|p| (p.pane_pid, p.pane_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn current_pane(&self) -> Option<String> {
        // `$ZELLIJ_PANE_ID` is set by zellij for every shell it spawns
        // inside a pane. Empty string is treated as unset to match
        // tmux's `current_pane` behaviour.
        std::env::var("ZELLIJ_PANE_ID")
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn focus_pane(&self, pane_id: &str) -> bool {
        // `zellij action focus-pane-with-id <id>` is documented and
        // stable. Best-effort: a non-zero exit (id no longer exists,
        // zellij server gone) collapses to `false` so callers can
        // show a "couldn't jump" hint.
        Command::new("zellij")
            .args(["action", "focus-pane-with-id", pane_id])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn ingest_pane_snapshot(&self, panes: Vec<PaneInfo>) {
        // The plugin's `PaneUpdate` push arrives via the daemon's
        // `BackendPaneSnapshot` IPC command. Replace the cache
        // wholesale — the plugin is the source of truth and always
        // sends the full pane set.
        self.replace_snapshot(panes);
    }

    fn caps(&self) -> BackendCaps {
        let has_fresh_snapshot = self
            .snapshot
            .read()
            .is_ok_and(|s| !s.fresh_panes().is_empty());
        BackendCaps {
            current_command: has_fresh_snapshot,
            pane_pid_map: has_fresh_snapshot,
            // Disruptive on CLI; plugin-only when non-disruptive.
            capture_pane: false,
            // Always works.
            focus_pane: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pane(id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session: "z".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: String::new(),
            title: String::new(),
            pane_pid: 0,
        }
    }

    /// `kind()` is the source-of-truth for which backend a record
    /// went through. Locked down so a future copy-paste of this impl
    /// can't accidentally inherit `HostKind::Tmux`.
    #[test]
    fn zellij_backend_reports_zellij_kind() {
        assert_eq!(ZellijBackend::new().kind(), HostKind::Zellij);
    }

    /// CLI-only baseline advertises plugin-only methods as
    /// unsupported, but `focus_pane` stays true. Locked down because
    /// flipping any of these to `true` without a corresponding code
    /// change is a silent regression — the caller would degrade
    /// thinking the result was just transiently empty.
    #[test]
    fn zellij_caps_match_cli_only_baseline() {
        let caps = ZellijBackend::new().caps();
        assert!(!caps.current_command);
        assert!(!caps.pane_pid_map);
        assert!(!caps.capture_pane);
        assert!(caps.focus_pane);
    }

    /// Empty cache → `list_panes` is empty and `resolve_pane` returns
    /// `None`. The backend is constructed in this state when no
    /// plugin has pushed yet; the test pins that behaviour.
    #[test]
    fn empty_cache_yields_empty_pane_inventory() {
        let b = ZellijBackend::new();
        assert!(b.list_panes().is_empty());
        assert!(b.resolve_pane("anything").is_none());
        assert!(b.pane_pid_map().is_empty());
        assert!(b.capture_pane("anything").is_none());
    }

    /// `replace_snapshot` round-trips: a wholesale swap is visible on
    /// the next read. This is the surface the future plugin pushes
    /// into; locking it now means the plugin can land without
    /// re-shaping the cache contract.
    #[test]
    fn replace_snapshot_updates_what_consumers_see() {
        let b = ZellijBackend::new();
        b.replace_snapshot(vec![fake_pane("z-1"), fake_pane("z-2")]);
        let panes = b.list_panes();
        assert_eq!(panes.len(), 2);
        assert_eq!(b.resolve_pane("z-1").unwrap().pane_id, "z-1");
        assert!(b.resolve_pane("missing").is_none());
    }

    /// `ingest_pane_snapshot` through a `dyn PaneBackend` routes to
    /// `replace_snapshot` for zellij — the exact path the daemon's
    /// `BackendPaneSnapshot` handler takes (it holds an
    /// `Arc<dyn PaneBackend>`, never the concrete type). Exercised via a
    /// trait object so a regression that drops the override — falling
    /// back to the no-op trait default — fails here.
    #[test]
    fn ingest_pane_snapshot_via_trait_updates_cache() {
        let b: Box<dyn PaneBackend> = Box::new(ZellijBackend::new());
        b.ingest_pane_snapshot(vec![fake_pane("z-7")]);
        assert_eq!(b.list_panes().len(), 1);
        assert_eq!(b.resolve_pane("z-7").unwrap().pane_id, "z-7");
    }

    /// Trait-object dispatch compiles — locks in the
    /// `Send + Sync + 'static` bounds the daemon relies on.
    #[test]
    fn zellij_backend_is_object_safe() {
        let _b: Box<dyn PaneBackend> = Box::new(ZellijBackend::new());
    }
}
