//! Periodic reconciliation against ground truth.
//!
//! The agent registry is event-driven: it accumulates state from hook
//! adapters, sync passes, and dashboard events, but has no inherent way to
//! discover that a pane has died, that a synthetic placeholder is no longer
//! authoritative, or that historical duplicates should be collapsed.
//! Without a reconciler the registry drifts over time — duplicate rows
//! pile up, ghost panes linger, and the picker view loses its 1:1
//! correspondence with the user's real tmux state.
//!
//! This module closes the loop by running a periodic, idempotent control
//! pass that treats tmux as the source of truth: observe → diff → converge,
//! the same shape as a Kubernetes controller. The reconciliation logic
//! itself lives on [`crate::state::Store::reconcile`]; this module is the
//! scheduler around it, plus the abstraction that makes it testable.
//!
//! ## Extensibility
//!
//! Liveness is fronted by the [`LivenessSource`] trait so the reconciler is
//! independent of tmux. Tests substitute a hand-rolled fake; future work
//! could plug in a multi-server tmux scanner, a screen/zellij adapter, or
//! a remote-host probe — all without touching `Reconciler` or `Store`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;

use crate::state::{ReconcileReport, SharedStore};
use crate::tmux::{self, PaneInfo};

/// Source of truth for which panes are currently alive.
///
/// Implementors return a snapshot of every pane the system considers live
/// at the moment of the call. `Send + Sync + 'static` so the reconciler can
/// own one inside a long-lived spawned task; `list_panes` is sync-blocking
/// because the production impl shells out to `tmux` and the reconciler
/// wraps the call in `spawn_blocking` itself.
pub trait LivenessSource: Send + Sync + 'static {
    fn list_panes(&self) -> Vec<PaneInfo>;
}

/// Production [`LivenessSource`] backed by `tmux list-panes -a`.
///
/// Failures (no tmux server, command error) collapse to an empty vec —
/// safer than panicking inside a background loop. An empty result lets the
/// reconciler reap stale records aggressively if tmux is gone, which is
/// usually what the user wants after a tmux crash.
pub struct TmuxLiveness;

impl LivenessSource for TmuxLiveness {
    fn list_panes(&self) -> Vec<PaneInfo> {
        tmux::list_panes().unwrap_or_default()
    }
}

/// Periodic control loop that converges the [`Store`](crate::state::Store)
/// against a [`LivenessSource`].
///
/// Generic over the source so unit tests substitute a fake without the
/// daemon ever touching tmux. The loop is idempotent — running it more
/// often costs CPU but cannot corrupt state — so the cadence is a tuning
/// knob, not a correctness requirement.
pub struct Reconciler<L: LivenessSource> {
    store: SharedStore,
    source: Arc<L>,
    interval: Duration,
}

impl<L: LivenessSource> Reconciler<L> {
    pub fn new(store: SharedStore, source: L, interval: Duration) -> Self {
        Self {
            store,
            source: Arc::new(source),
            interval,
        }
    }

    /// Run a single reconciliation pass on demand. Useful for tests, for
    /// surfacing a "force reconcile" CLI command later, and for triggering
    /// a pass right after startup discovery so the user doesn't wait a full
    /// tick to see a clean view.
    pub async fn reconcile_once(&self) -> ReconcileReport {
        // `list_panes` shells out to tmux and must not block the runtime.
        let src = self.source.clone();
        let panes = tokio::task::spawn_blocking(move || src.list_panes())
            .await
            .unwrap_or_default();
        self.store.reconcile(&panes).await
    }

    /// Run the periodic loop until `shutdown` fires.
    ///
    /// `MissedTickBehavior::Skip` keeps the cadence honest under load: if a
    /// reconciliation pass takes longer than the interval (rare but
    /// possible on a stalled tmux server), we resume on the next aligned
    /// tick rather than stacking up an unbounded queue of catch-up passes.
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        let mut tick = interval(self.interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; skip it so the loop's cadence
        // matches the configured interval rather than running once at t=0.
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let report = self.reconcile_once().await;
                    if !report.is_noop() {
                        debug!(
                            stale = report.stale_panes_reaped,
                            synthetic = report.synthetic_demoted,
                            duplicates = report.duplicates_collapsed,
                            "reconciler swept registry",
                        );
                    }
                }
                _ = shutdown.recv() => {
                    debug!("reconciler shutting down");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind};
    use crate::state::Store;
    use std::sync::Mutex;
    use time::macros::datetime;

    /// In-memory fake [`LivenessSource`] used to drive deterministic tests
    /// without touching tmux. Wrapped in a `Mutex` so callers can mutate
    /// the live set between reconciliation passes.
    struct FakeLiveness {
        panes: Mutex<Vec<PaneInfo>>,
    }

    impl FakeLiveness {
        fn new(panes: Vec<PaneInfo>) -> Self {
            Self {
                panes: Mutex::new(panes),
            }
        }
        fn set(&self, panes: Vec<PaneInfo>) {
            *self.panes.lock().unwrap() = panes;
        }
    }

    impl LivenessSource for FakeLiveness {
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.lock().unwrap().clone()
        }
    }

    /// Newtype that lets the loop test pass a shared `Arc<FakeLiveness>` to
    /// the runner while still mutating the live set from the outside via
    /// the original `Arc`. Owning a sole `FakeLiveness` would force us to
    /// snapshot live panes once at construction.
    struct ArcLiveness(Arc<FakeLiveness>);

    impl LivenessSource for ArcLiveness {
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.0.list_panes()
        }
    }

    fn pane(id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session: "s".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
        }
    }

    fn started(sid: &str, pane_id: &str, at: time::OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                pane: Some(pane_id.into()),
                cwd: None,
            },
            at,
        }
    }

    #[tokio::test]
    async fn reconcile_once_reaps_dead_pane_via_fake_source() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&started("a", "%1", t0)).await;
        store.apply(&started("b", "%2", t0)).await;
        let fake = FakeLiveness::new(vec![pane("%1")]);

        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10));
        let report = r.reconcile_once().await;

        assert_eq!(report.stale_panes_reaped, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "a");
    }

    #[tokio::test]
    async fn run_loop_converges_then_exits_on_shutdown() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&started("ghost", "%99", t0)).await;
        let fake = Arc::new(FakeLiveness::new(vec![pane("%1")]));
        let fake_for_runner = Arc::clone(&fake);

        let (tx, _) = broadcast::channel::<()>(1);
        let rx = tx.subscribe();
        // Tiny interval so the first scheduled tick fires fast in tests.
        let runner = Reconciler::new(
            store.clone(),
            ArcLiveness(fake_for_runner),
            Duration::from_millis(20),
        );
        let task = tokio::spawn(runner.run(rx));

        // Give the loop a couple of ticks to converge.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            store.snapshot().await.len(),
            0,
            "ghost agent should have been reaped",
        );

        // Add another stale entry, expand live set, and verify next tick
        // converges again — proves idempotent re-entry.
        store.apply(&started("ghost2", "%88", t0)).await;
        fake.set(vec![pane("%1"), pane("%88")]);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "second ghost should also be reaped");
        assert_eq!(snap[0].session_id, "ghost2");

        let _ = tx.send(());
        // Task must observe shutdown; bounded wait so the test doesn't hang.
        let exited = tokio::time::timeout(Duration::from_millis(500), task).await;
        assert!(exited.is_ok(), "reconciler did not honor shutdown");
    }
}
