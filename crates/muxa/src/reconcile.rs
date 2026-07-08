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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;

use crate::adapters::codex_rollout;
use crate::event::{AgentEvent, AgentId, AgentKind, AgentState, RateLimitScope, RateLimitSource};
use crate::metrics::Metrics;
use crate::process_tree;
use crate::state::{ReconcileReport, SharedStore};
use crate::tmux::PaneInfo;

/// How many days back from "now" to scan codex's date-partitioned sessions
/// tree when locating a live session's rollout file. Today + yesterday
/// covers any session a human is actively driving; older active sessions
/// are rare enough to skip rather than pay a wider directory scan. (The
/// scan also looks one day *forward* — see [`codex_rollout::locate_rollout`]
/// — to cover timezones where the local rollout date is ahead of UTC.)
const ROLLOUT_LOOKBACK_DAYS: u16 = 1;

/// A live codex row snapshotted with its current rate-limit fields, captured
/// before the off-runtime rollout read so [`Reconciler::poll_codex_rollouts`]
/// can suppress no-op re-emissions (a `Heartbeat`/`RateLimited` that wouldn't
/// change the row).
struct CodexPollTarget {
    id: AgentId,
    cur_5h_pct: Option<f32>,
    cur_5h_reset: Option<OffsetDateTime>,
    cur_7d_pct: Option<f32>,
    cur_7d_reset: Option<OffsetDateTime>,
    state: AgentState,
    scope: Option<RateLimitScope>,
    source: Option<RateLimitSource>,
}

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
    /// Root of codex's session-rollout tree (`~/.codex/sessions`). When
    /// `Some`, each tick reads every live codex row's rollout file and
    /// feeds its `rate_limits` through the store — the only way muxa learns
    /// a codex usage cap, since codex exposes no error/rate-limit hook.
    /// `None` (default) disables the poll.
    codex_sessions_root: Option<PathBuf>,
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
            codex_sessions_root: None,
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

    /// Enable per-tick polling of codex session-rollout files for
    /// rate-limit state. `root` is codex's sessions tree
    /// (`~/.codex/sessions`). `None` (the default) keeps the poll off, so
    /// non-codex deployments pay nothing.
    #[must_use]
    pub fn with_codex_sessions_root(mut self, root: Option<PathBuf>) -> Self {
        self.codex_sessions_root = root;
        self
    }

    /// Read every live codex row's rollout file and feed its `rate_limits`
    /// into the store. Codex ships no error/rate-limit hook, so this poll is
    /// muxa's only path to a codex usage cap — including the common case
    /// where the cap blocks a turn *before it starts* and not even a `Stop`
    /// hook fires.
    ///
    /// Two events per reading:
    /// * a `Heartbeat` carrying the 5h/7d utilization (keeps the LIMITS
    ///   column live and gives the existing soft-saturation handling), and
    /// * a `RateLimited` when codex stamped `rate_limit_reached_type` —
    ///   the hard signal that flips the row to `Error`.
    ///
    /// File IO runs on the blocking pool; absent files / unreadable rollouts
    /// are silently skipped (a session that hasn't written a `rate_limits`
    /// record yet just contributes nothing this tick).
    async fn poll_codex_rollouts(&self, root: &Path) {
        let now = OffsetDateTime::now_utc();
        // Snapshot the live codex rows first (cheap, async), then do the
        // file reads off-runtime. Stopped rows can't be rate-limited. We
        // carry each row's current rate-limit fields so we can emit only on
        // change (see the no-op guards below).
        let targets: Vec<CodexPollTarget> = self
            .store
            .snapshot()
            .await
            .into_iter()
            .filter(|a| a.kind == AgentKind::Codex && a.state != AgentState::Stopped)
            .map(|a| CodexPollTarget {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: a.session_id,
                    surface: a.surface,
                    pane: a.pane,
                    cwd: a.cwd,
                },
                cur_5h_pct: a.rate_limit_5h_pct,
                cur_5h_reset: a.rate_limit_5h_resets_at,
                cur_7d_pct: a.rate_limit_7d_pct,
                cur_7d_reset: a.rate_limit_7d_resets_at,
                state: a.state,
                scope: a.rate_limit_scope,
                source: a.rate_limit_source,
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        let root = root.to_path_buf();
        let readings = tokio::task::spawn_blocking(move || {
            targets
                .into_iter()
                .filter_map(|t| {
                    codex_rollout::session_rate_limits(
                        &root,
                        &t.id.session_id,
                        now,
                        ROLLOUT_LOOKBACK_DAYS,
                    )
                    .map(|rl| (t, rl))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        for (t, rl) in readings {
            // `Window` is `Copy`, so read each scope's fields straight off
            // `rl` rather than aliasing into similarly-named locals.
            let five = rl.five_hour;
            let seven = rl.seven_day;

            // Emit a Heartbeat only when a reading actually moved. Every
            // `apply()` refreshes `last_activity_at`, and `mark_stuck_idle_from`
            // skips any row whose `last_activity_at` is newer than its cutoff —
            // so a no-op Heartbeat every tick would permanently defeat the
            // `stuck_working_timeout` / `stuck_waiting_timeout` recovery for
            // codex, the very agent those sweeps target. Suppressing unchanged
            // readings also avoids waking the snapshotter each tick.
            let hb_changed = five.map(|w| w.used_percent) != t.cur_5h_pct
                || five.and_then(|w| w.resets_at) != t.cur_5h_reset
                || seven.map(|w| w.used_percent) != t.cur_7d_pct
                || seven.and_then(|w| w.resets_at) != t.cur_7d_reset;
            if hb_changed {
                self.store
                    .apply(&AgentEvent::Heartbeat {
                        id: t.id.clone(),
                        model: None,
                        context_used_pct: None,
                        cost_usd: None,
                        rate_limit_5h_pct: five.map(|w| w.used_percent),
                        rate_limit_5h_resets_at: five.and_then(|w| w.resets_at),
                        rate_limit_7d_pct: seven.map(|w| w.used_percent),
                        rate_limit_7d_resets_at: seven.and_then(|w| w.resets_at),
                        at: now,
                    })
                    .await;
            }

            if let Some(scope) = rl.reached {
                // Re-assert the cap only when the row isn't already showing
                // this exact codex cap. It persists in the store once set, so
                // re-emitting every tick would only refresh `last_activity_at`
                // (same sweep-defeating problem as above).
                let already_capped = t.state == AgentState::Error
                    && t.scope == Some(scope)
                    && t.source == Some(RateLimitSource::CodexRollout);
                if !already_capped {
                    // Reset time comes from whichever window actually tripped.
                    let resets_at = match scope {
                        RateLimitScope::SevenDay => seven.and_then(|w| w.resets_at),
                        RateLimitScope::FiveHour | RateLimitScope::Unknown => {
                            five.and_then(|w| w.resets_at)
                        }
                    };
                    self.store
                        .apply(&AgentEvent::RateLimited {
                            id: t.id,
                            scope,
                            source: RateLimitSource::CodexRollout,
                            resets_at,
                            message: None,
                            at: now,
                        })
                        .await;
                }
            }
        }
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
        let list_started = Instant::now();
        let panes = tokio::task::spawn_blocking(move || src.list_panes())
            .await
            .unwrap_or_default();
        let list_panes_us = u64::try_from(list_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let panes_for_workload = panes.clone();
        let workload_started = Instant::now();
        let workloads = tokio::task::spawn_blocking(move || {
            process_tree::scan_pane_workloads(&panes_for_workload)
        })
        .await
        .unwrap_or_default();
        let workload_scan_us =
            u64::try_from(workload_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let reconcile_started = Instant::now();
        let report = self.store.reconcile(&panes).await;
        let workload_changed = self.store.update_workloads(&workloads).await;
        let store_update_us =
            u64::try_from(reconcile_started.elapsed().as_micros()).unwrap_or(u64::MAX);
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
        // Poll codex rollouts for rate-limit state (no-op unless a sessions
        // root is configured). Codex has no rate-limit hook, so this is the
        // only signal — and it's the only path that catches a cap which
        // blocked a turn before any hook could fire.
        if let Some(root) = &self.codex_sessions_root {
            self.poll_codex_rollouts(root).await;
        }
        // Flip pid-tracked task rows whose process has exited to Stopped.
        let dead_tasks = self.store.reap_dead_pids().await;
        if dead_tasks > 0 {
            tracing::info!("pid-liveness sweep stopped {dead_tasks} dead task row(s)");
        }
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
            list_panes_us,
            workload_scan_us,
            store_update_us,
            panes = panes.len(),
            workloads = workloads.len(),
            stale = report.stale_panes_reaped,
            synthetic = report.synthetic_demoted,
            duplicates = report.duplicates_collapsed,
            workload_changed,
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
            pid: None,
            workload: crate::WorkloadSummary::default(),
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

    // --- codex rollout polling ------------------------------------------

    fn codex_started(sid: &str, pane_id: &str, at: time::OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::Codex,
                session_id: sid.into(),
                surface: None,
                pane: Some(pane_id.into()),
                cwd: None,
            },
            at,
        }
    }

    /// Write a real-shape codex rollout for `session_id` under
    /// `root/YYYY/MM/DD` for *today* (the poll resolves the date from the
    /// wall clock), with one `token_count` line carrying the given windows.
    fn write_codex_rollout(
        root: &std::path::Path,
        session_id: &str,
        primary_pct: f32,
        reached: &str,
    ) {
        let now = time::OffsetDateTime::now_utc();
        let day = root
            .join(format!("{:04}", now.year()))
            .join(format!("{:02}", u8::from(now.month())))
            .join(format!("{:02}", now.day()));
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-06-12T15-24-44-{session_id}.jsonl"));
        let line = format!(
            r#"{{"timestamp":"2026-06-12T06:26:08.491Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"primary":{{"used_percent":{primary_pct},"window_minutes":300,"resets_at":1781262859}},"secondary":{{"used_percent":46.0,"window_minutes":10080,"resets_at":1781745469}},"rate_limit_reached_type":{reached}}}}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();
    }

    /// A reached cap in the rollout flips the codex row to `Error` with a
    /// `CodexRollout` source — the no-hook signal the daemon would otherwise
    /// miss entirely.
    #[tokio::test]
    async fn reconcile_polls_codex_rollout_and_flips_to_error_on_reached_cap() {
        use crate::event::{RateLimitScope, RateLimitSource};
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&codex_started("cdx", "%7", t0)).await;

        let root = tempfile::tempdir().unwrap();
        write_codex_rollout(root.path(), "cdx", 100.0, r#""primary""#);

        // Codex's pane must stay live or the reconcile pass reaps the row
        // before the poll runs.
        let mut p = pane("%7");
        p.current_command = "codex".into();
        let fake = FakeLiveness::new(vec![p]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10))
            .with_codex_sessions_root(Some(root.path().to_path_buf()));
        r.reconcile_once().await;

        let snap = store.snapshot().await;
        let agent = snap.iter().find(|a| a.session_id == "cdx").expect("row");
        assert_eq!(agent.state, AgentState::Error);
        assert_eq!(agent.rate_limit_scope, Some(RateLimitScope::FiveHour));
        assert_eq!(agent.rate_limit_source, Some(RateLimitSource::CodexRollout));
        assert_eq!(agent.rate_limit_5h_pct, Some(100.0));
    }

    /// Credit-plan exhaustion (windows null, `credits.has_credits:false`)
    /// must also flip the row to `Error`. This is the real shape that left a
    /// rate-limited codex session showing `working` before the fix.
    #[tokio::test]
    async fn reconcile_flips_to_error_on_credit_exhaustion() {
        use crate::event::{RateLimitScope, RateLimitSource};
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&codex_started("cdxc", "%11", t0)).await;

        let now = time::OffsetDateTime::now_utc();
        let root = tempfile::tempdir().unwrap();
        let day = root
            .path()
            .join(format!("{:04}", now.year()))
            .join(format!("{:02}", u8::from(now.month())))
            .join(format!("{:02}", now.day()));
        std::fs::create_dir_all(&day).unwrap();
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"premium","primary":null,"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"rate_limit_reached_type":null}}}"#;
        std::fs::write(
            day.join("rollout-2026-06-12T15-24-44-cdxc.jsonl"),
            format!("{line}\n"),
        )
        .unwrap();

        let mut p = pane("%11");
        p.current_command = "codex".into();
        let fake = FakeLiveness::new(vec![p]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10))
            .with_codex_sessions_root(Some(root.path().to_path_buf()));
        r.reconcile_once().await;

        let snap = store.snapshot().await;
        let agent = snap.iter().find(|a| a.session_id == "cdxc").expect("row");
        assert_eq!(agent.state, AgentState::Error);
        assert_eq!(agent.rate_limit_scope, Some(RateLimitScope::Unknown));
        assert_eq!(agent.rate_limit_source, Some(RateLimitSource::CodexRollout));
    }

    /// A rollout with utilization but no reached cap fills the percentage
    /// columns without flipping the row's state (Heartbeat semantics).
    #[tokio::test]
    async fn reconcile_polls_codex_rollout_percentages_without_state_change() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&codex_started("cdx2", "%8", t0)).await;

        let root = tempfile::tempdir().unwrap();
        write_codex_rollout(root.path(), "cdx2", 42.0, "null");

        let mut p = pane("%8");
        p.current_command = "codex".into();
        let fake = FakeLiveness::new(vec![p]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10))
            .with_codex_sessions_root(Some(root.path().to_path_buf()));
        r.reconcile_once().await;

        let snap = store.snapshot().await;
        let agent = snap.iter().find(|a| a.session_id == "cdx2").expect("row");
        // Idle (from Started) — Heartbeat doesn't drive state transitions.
        assert_eq!(agent.state, AgentState::Idle);
        assert_eq!(agent.rate_limit_5h_pct, Some(42.0));
        assert_eq!(agent.rate_limit_7d_pct, Some(46.0));
        assert!(agent.rate_limit_scope.is_none());
    }

    /// An unchanged rollout reading must NOT re-emit events: every `apply()`
    /// refreshes `last_activity_at`, which would permanently defeat the
    /// stuck-state sweep for codex. Two passes over an identical rollout must
    /// leave `last_activity_at` untouched after the first.
    #[tokio::test]
    async fn reconcile_codex_rollout_unchanged_reading_does_not_refresh_activity() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&codex_started("cdx4", "%10", t0)).await;

        let root = tempfile::tempdir().unwrap();
        write_codex_rollout(root.path(), "cdx4", 42.0, "null");

        let mut p = pane("%10");
        p.current_command = "codex".into();
        let fake = FakeLiveness::new(vec![p]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10))
            .with_codex_sessions_root(Some(root.path().to_path_buf()));

        // First pass: the reading is new, so a Heartbeat lands and bumps
        // last_activity_at.
        r.reconcile_once().await;
        let la1 = store
            .snapshot()
            .await
            .into_iter()
            .find(|a| a.session_id == "cdx4")
            .unwrap()
            .last_activity_at;

        // Second pass: identical reading → no event → activity clock frozen.
        r.reconcile_once().await;
        let la2 = store
            .snapshot()
            .await
            .into_iter()
            .find(|a| a.session_id == "cdx4")
            .unwrap()
            .last_activity_at;

        assert_eq!(
            la1, la2,
            "unchanged rollout must not refresh last_activity_at"
        );
    }

    /// With no sessions root configured the poll is inert — non-codex
    /// deployments (and the historical default) pay nothing and see no
    /// codex-specific behavior.
    #[tokio::test]
    async fn reconcile_without_codex_root_does_not_poll() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&codex_started("cdx3", "%9", t0)).await;

        let mut p = pane("%9");
        p.current_command = "codex".into();
        let fake = FakeLiveness::new(vec![p]);
        let r = Reconciler::new(store.clone(), fake, Duration::from_millis(10));
        r.reconcile_once().await;

        let snap = store.snapshot().await;
        let agent = snap.iter().find(|a| a.session_id == "cdx3").expect("row");
        assert_eq!(agent.state, AgentState::Idle);
        assert!(agent.rate_limit_5h_pct.is_none());
    }
}
