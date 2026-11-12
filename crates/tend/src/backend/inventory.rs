//! A shared, short-lived pane inventory for the daemon's periodic tasks.
//!
//! Screen detection (every 3 s), the pane→session cache (5 s), Fleet's local
//! snapshot (15 s) and periodic discovery (30 s) each used to run their own
//! `list-panes` against every pane host. [`InventoryBackend`] wraps one
//! backend so those tasks share a single listing: a call within the TTL of
//! the previous listing reuses it, and concurrent callers wait for the one
//! listing in flight instead of starting their own (single-flight).
//!
//! Only [`PaneBackend::list_panes`] is cached. Everything else — including
//! [`PaneBackend::observe_panes`], which liveness reconciliation treats as
//! authoritative, and every capture, send and mutation — delegates to the
//! wrapped backend unchanged. Request paths (IPC, launches) keep using the
//! unwrapped backends, so a pane created a moment ago is always visible to
//! the request that needs it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{BackendCaps, HostKind, PaneBackend, PaneObservation, SharedBackend};
use crate::tmux::PaneInfo;

/// How long one listing serves later callers. Shorter than the fastest
/// periodic caller (screen detection's 3 s tick), so that caller still sees
/// a listing taken this tick, while slower callers ride along on it.
pub const DEFAULT_INVENTORY_TTL: Duration = Duration::from_millis(2500);

/// A [`PaneBackend`] whose `list_panes` is shared for [`Self::ttl`].
pub struct InventoryBackend {
    inner: SharedBackend,
    ttl: Duration,
    /// Held across a refresh: that is the single-flight. Callers run on
    /// blocking threads (every periodic caller lists inside
    /// `spawn_blocking`), so waiting here never stalls an async worker.
    cached: Mutex<Option<(Instant, Vec<PaneInfo>)>>,
}

impl InventoryBackend {
    pub fn new(inner: SharedBackend, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cached: Mutex::new(None),
        }
    }

    /// Wrap each backend, preserving order.
    pub fn share(backends: &[SharedBackend], ttl: Duration) -> Vec<SharedBackend> {
        backends
            .iter()
            .map(|backend| Arc::new(Self::new(backend.clone(), ttl)) as SharedBackend)
            .collect()
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn invalidate(&self) {
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl std::fmt::Debug for InventoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InventoryBackend")
            .field("kind", &self.inner.kind())
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl PaneBackend for InventoryBackend {
    fn kind(&self) -> HostKind {
        self.inner.kind()
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, panes)) = cached.as_ref() {
            if Instant::now().saturating_duration_since(*at) < self.ttl {
                return panes.clone();
            }
        }
        let panes = self.inner.list_panes();
        // Stamp after the listing: the TTL measures how old the data is.
        *cached = Some((Instant::now(), panes.clone()));
        panes
    }
    fn observe_panes(&self) -> PaneObservation {
        self.inner.observe_panes()
    }
    fn work_panes(&self) -> Option<Vec<(PaneInfo, crate::tmux::scanner::TendPaneMetadata)>> {
        self.inner.work_panes()
    }
    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.inner.resolve_pane(pane_id)
    }
    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        self.inner.capture_pane(pane_id)
    }
    fn pane_pid_map(&self) -> HashMap<u32, String> {
        self.inner.pane_pid_map()
    }
    fn current_pane(&self) -> Option<String> {
        self.inner.current_pane()
    }
    fn focus_pane(&self, pane_id: &str) -> bool {
        self.inner.focus_pane(pane_id)
    }
    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.inner.send_text(pane_id, text)
    }
    fn send_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        self.inner.send_text_on(socket, pane_id, text)
    }
    fn send_prompt_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        self.inner.send_prompt_text_on(socket, pane_id, text)
    }
    fn capture_pane_on(&self, socket: Option<&str>, pane_id: &str) -> Option<String> {
        self.inner.capture_pane_on(socket, pane_id)
    }
    fn capture_pane_history_on(
        &self,
        socket: Option<&str>,
        pane_id: &str,
        history: usize,
    ) -> Option<String> {
        self.inner.capture_pane_history_on(socket, pane_id, history)
    }
    fn caps(&self) -> BackendCaps {
        self.inner.caps()
    }
    fn remain_on_exit_override(
        &self,
        socket: Option<&str>,
        pane: &str,
    ) -> Result<Option<String>, String> {
        self.inner.remain_on_exit_override(socket, pane)
    }
    fn restore_remain_on_exit(
        &self,
        socket: Option<&str>,
        pane: &str,
        value: Option<&str>,
    ) -> bool {
        self.inner.restore_remain_on_exit(socket, pane, value)
    }
    fn set_remain_on_exit_on(&self, socket: Option<&str>, pane_id: &str, on: bool) -> bool {
        self.inner.set_remain_on_exit_on(socket, pane_id, on)
    }
    fn respawn_pane_on(
        &self,
        socket: Option<&str>,
        pane_id: &str,
        cwd: &str,
        command: &str,
    ) -> bool {
        let respawned = self.inner.respawn_pane_on(socket, pane_id, cwd, command);
        self.invalidate();
        respawned
    }
    fn ingest_pane_snapshot(&self, panes: Vec<PaneInfo>) {
        self.inner.ingest_pane_snapshot(panes);
        self.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        lists: AtomicUsize,
        delay: Duration,
    }

    impl PaneBackend for Counting {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            std::thread::sleep(self.delay);
            self.lists.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
        fn observe_panes(&self) -> PaneObservation {
            self.lists.fetch_add(100, Ordering::SeqCst);
            PaneObservation::complete(Vec::new())
        }
        fn resolve_pane(&self, _: &str) -> Option<PaneInfo> {
            None
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            None
        }
        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
    }

    fn counting(delay: Duration) -> Arc<Counting> {
        Arc::new(Counting {
            lists: AtomicUsize::new(0),
            delay,
        })
    }

    #[test]
    fn a_listing_is_reused_within_the_ttl_only() {
        let inner = counting(Duration::ZERO);
        let shared = InventoryBackend::new(inner.clone(), Duration::from_millis(50));
        shared.list_panes();
        shared.list_panes();
        assert_eq!(inner.lists.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(60));
        shared.list_panes();
        assert_eq!(inner.lists.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn concurrent_callers_share_one_listing_in_flight() {
        let inner = counting(Duration::from_millis(50));
        let shared = Arc::new(InventoryBackend::new(inner.clone(), Duration::from_secs(5)));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || shared.list_panes())
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(inner.lists.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn observation_is_never_served_from_the_cache() {
        let inner = counting(Duration::ZERO);
        let shared = InventoryBackend::new(inner.clone(), Duration::from_secs(5));
        shared.list_panes();
        shared.observe_panes();
        shared.observe_panes();
        assert_eq!(inner.lists.load(Ordering::SeqCst), 201);
    }

    #[test]
    fn an_ingested_snapshot_invalidates_the_cached_listing() {
        let inner = counting(Duration::ZERO);
        let shared = InventoryBackend::new(inner.clone(), Duration::from_secs(5));
        shared.list_panes();
        shared.ingest_pane_snapshot(Vec::new());
        shared.list_panes();
        assert_eq!(inner.lists.load(Ordering::SeqCst), 2);
    }
}
