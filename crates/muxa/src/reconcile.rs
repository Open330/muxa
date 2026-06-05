//! **Internal:** this module's items are `#[doc(hidden)]` — they're
//! `pub` for the workspace binaries (`muxad`) but excluded from
//! the public API surface and semver-exempt.
//!
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
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;

use crate::metrics::Metrics;
use crate::state::{ReconcileReport, SharedStore};
use crate::tmux::PaneInfo;

/// Source of truth for which panes are currently alive.
///
/// Implementors return a snapshot of every pane the system considers live
/// at the moment of the call. `Send + Sync + 'static` so the reconciler can
/// own one inside a long-lived spawned task; `list_panes` is sync-blocking
/// because the production impl shells out to `tmux` and the reconciler
/// wraps the call in `spawn_blocking` itself.
///
/// Every [`PaneBackend`](crate::backend::PaneBackend) is automatically a
/// `LivenessSource` via the blanket impl below — backends don't need to
/// re-spell their `list_panes` for the reconciler. Tests that want to
/// drive the reconciler with a hand-rolled fake can either implement
/// `LivenessSource` directly (lightweight) or implement `PaneBackend`
/// and inherit the blanket impl (more realistic).
pub trait LivenessSource: Send + Sync + 'static {
    fn list_panes(&self) -> Vec<PaneInfo>;
}

/// Every pane backend is a liveness source. Saves every backend impl
/// from repeating a one-line delegation, and keeps the reconciler
/// integration colocated with the trait whose contract it leans on.
impl<B: crate::backend::PaneBackend> LivenessSource for B {
    fn list_panes(&self) -> Vec<PaneInfo> {
        crate::backend::PaneBackend::list_panes(self)
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
    /// Optional metrics handle. Daemon wires one in via
    /// [`Self::with_metrics`]; tests can leave it `None` to avoid
    /// plumbing through a `Metrics` they never inspect.
    metrics: Option<Metrics>,
    /// If non-zero, agents stuck in `Working` for this long get
    /// auto-flipped to `Idle` on every tick. `Duration::ZERO`
    /// (default) disables the sweep so the historical
    /// "state-on-events-only" semantics are preserved.
    stuck_working_timeout: Duration,
    /// Same shape as `stuck_working_timeout` but for `WaitingInput`.
    /// Covers Codex's permission-grant case where the row gets
    /// pinned yellow with no follow-up hook to recover from.
    stuck_waiting_timeout: Duration,
}

impl<L: LivenessSource> Reconciler<L> {
    pub fn new(store: SharedStore, source: L, interval: Duration) -> Self {
        Self {
            store,
            source: Arc::new(source),
            interval,
            metrics: None,
            stuck_working_timeout: Duration::ZERO,
            stuck_waiting_timeout: Duration::ZERO,
        }
    }

    /// Attach a runtime [`Metrics`] handle so the reconciler bumps
    /// `reconcile_passes_total` after every pass.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Enable auto-downgrade of stuck `Working` agents to `Idle`.
    /// `Duration::ZERO` (the default) keeps the sweep off.
    #[must_use]
    pub fn with_stuck_working_timeout(mut self, t: Duration) -> Self {
        self.stuck_working_timeout = t;
        self
    }

    /// Enable auto-downgrade of stuck `WaitingInput` agents to
    /// `Idle`. Used to recover Codex rows that get pinned to
    /// `WaitingInput` after the user grants permission and the
    /// agent resumes without firing a follow-up hook.
    /// `Duration::ZERO` (the default) keeps the sweep off.
    #[must_use]
    pub fn with_stuck_waiting_timeout(mut self, t: Duration) -> Self {
        self.stuck_waiting_timeout = t;
        self
    }

    /// Run a single reconciliation pass on demand. Useful for tests, for
    /// surfacing a "force reconcile" CLI command later, and for triggering
    /// a pass right after startup discovery so the user doesn't wait a full
    /// tick to see a clean view.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn reconcile_once(&self) -> ReconcileReport {
        let started = Instant::now();
        // `list_panes` shells out to tmux and must not block the runtime.
        let src = self.source.clone();
        let panes = tokio::task::spawn_blocking(move || src.list_panes())
            .await
            .unwrap_or_default();
        let report = self.store.reconcile(&panes).await;
        let stuck_w = self
            .store
            .mark_stuck_idle_from(
                crate::event::AgentState::Working,
                self.stuck_working_timeout,
            )
            .await;
        let stuck_input = self
            .store
            .mark_stuck_idle_from(
                crate::event::AgentState::WaitingInput,
                self.stuck_waiting_timeout,
            )
            .await;
        let stuck_choice = self
            .store
            .mark_stuck_idle_from(
                crate::event::AgentState::WaitingChoice,
                self.stuck_waiting_timeout,
            )
            .await;
        if let Some(m) = &self.metrics {
            m.record_reconcile_pass();
        }
        let stuck_total = stuck_w + stuck_input + stuck_choice;
        if stuck_total > 0 {
            tracing::info!(
                working = stuck_w,
                waiting_input = stuck_input,
                waiting_choice = stuck_choice,
                "stuck-state sweep flipped {stuck_total} agent(s) to Idle",
            );
        }
        // Always emit the timing line at debug (cheap, off by default)
        // even on no-op passes — operators want to see the loop is alive
        // when investigating a stuck reconciler.
        debug!(
            elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            panes = panes.len(),
            stale = report.stale_panes_reaped,
            synthetic = report.synthetic_demoted,
            duplicates = report.duplicates_collapsed,
            "reconciler.tick",
        );
        report
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
                    // `reconcile_once` already emits a debug-level
                    // `reconciler.tick` line with timing + report
                    // breakdown; no need to log again here.
                    let _ = self.reconcile_once().await;
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
    //! Reconciler test scope.
    //!
    //! Synthetic-to-real promotion happens at apply-time inside
    //! `Store::apply` (real `Started` evicts a synthetic on the same pane
    //! before the row ever reaches the registry's persistent state) — it is
    //! NOT a `reconcile_once` responsibility. That contract is covered by
    //! `state.rs::tests::real_started_replaces_synthetic_on_same_pane`, so
    //! we deliberately do not duplicate it here.
    //!
    //! What this module DOES cover for the reconciler:
    //! - reaping rows whose pane is no longer alive,
    //! - collapsing duplicate real rows on the same live pane,
    //! - demoting orphan synthetics that somehow coexist with a real row
    //!   (defense-in-depth, planted via the public `Store::hydrate` seam).
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
            pane_pid: 0,
        }
    }

    fn started(sid: &str, pane_id: &str, at: time::OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                surface: None,
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

    /// Two real `Started`s on the same pane (e.g. user closed the agent
    /// and relaunched without `SessionEnded` ever firing) leave the store
    /// holding two records — the older flipped to `Stopped` by
    /// `Store::apply`'s pane-occupancy reconciliation. The periodic
    /// reconciler must collapse these onto the canonical (alive, most
    /// recent) row when the pane is still live.
    #[tokio::test]
    async fn reconcile_once_collapses_duplicates_for_same_live_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:05:00 UTC);

        store.apply(&started("first", "%1", t0)).await;
        store.apply(&started("second", "%1", t1)).await;
        // Sanity: both records are present pre-reconcile — `Store::apply`
        // only flips the older to `Stopped`, it doesn't evict.
        assert_eq!(store.snapshot().await.len(), 2);

        let fake = FakeLiveness::new(vec![pane("%1")]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10));
        let report = r.reconcile_once().await;

        assert_eq!(report.stale_panes_reaped, 0);
        assert_eq!(report.synthetic_demoted, 0);
        assert_eq!(
            report.duplicates_collapsed, 1,
            "the older `Stopped` real row should have been collapsed",
        );
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "second");
    }

    // NOTE: a previous revision of this file carried a
    // `reconcile_once_observes_synthetic_to_real_promotion` test that
    // asserted a noop reconcile pass after a real `Started` had already
    // evicted a synthetic at apply-time. It was a tautology — promotion
    // is a `Store::apply` responsibility, not a `reconcile_once` one — and
    // is now covered honestly by
    // `state.rs::tests::real_started_replaces_synthetic_on_same_pane`. See
    // the module-level comment above for the reconciler's actual scope.

    /// Defense-in-depth: even if a synthetic somehow ends up coexisting
    /// with a real entry on the same pane (e.g. an order-of-arrival edge
    /// case or a future code path that bypasses `apply`'s pane-reconcile),
    /// the reconciler must demote the synthetic on its next pass. We use
    /// the public `hydrate` seam to plant both rows without going through
    /// `Store::apply`'s pane-occupancy reconciliation.
    #[tokio::test]
    async fn reconcile_once_demotes_orphan_synthetic_via_reconciler() {
        use crate::event::AgentState;
        use crate::state::Agent;
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);

        let mk = |sid: &str, state: AgentState| Agent {
            kind: AgentKind::ClaudeCode,
            session_id: sid.into(),
            surface: None,
            pane: Some("%3".into()),
            cwd: None,
            state,
            last_prompt: None,
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: t0,
            last_activity_at: t0,
            state_entered_at: t0,
        };
        store
            .hydrate(vec![
                mk("real", AgentState::Idle),
                mk("synthetic-%3", AgentState::Idle),
            ])
            .await;
        assert_eq!(store.snapshot().await.len(), 2);

        let fake = FakeLiveness::new(vec![pane("%3")]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10));
        let report = r.reconcile_once().await;

        assert_eq!(report.stale_panes_reaped, 0);
        assert_eq!(report.synthetic_demoted, 1);
        assert_eq!(report.duplicates_collapsed, 0);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "real");
    }
}
