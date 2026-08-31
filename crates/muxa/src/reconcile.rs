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
use tracing::{debug, warn};

use crate::adapters::codex_rollout;
use crate::backend::{HostKind, PaneObservation};
use crate::event::{AgentEvent, AgentId, AgentKind, AgentState, RateLimitScope, RateLimitSource};
use crate::metrics::Metrics;
use crate::process_tree;
use crate::state::{ReconcileReport, SharedStore};
#[cfg(test)]
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
/// at the moment of the call, including whether that snapshot is complete
/// enough to use as negative liveness evidence. `Send + Sync + 'static` so
/// the reconciler can own one inside a long-lived spawned task;
/// `observe_panes` is sync-blocking
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
    fn observe_panes(&self) -> PaneObservation;

    /// Which host produced this source's observations. The reconciler threads
    /// it into [`Store::reconcile_observation`](crate::state::Store::reconcile_observation)
    /// so the cross-host reaping guard only reaps rows whose pane id belongs to
    /// the observing host — a tmux daemon must not reap live `herdr:`/`zellij:`
    /// rows, and vice versa. Defaults to [`HostKind::Tmux`] for the hand-rolled
    /// test fakes that predate the guard and only ever carry `%N` panes.
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }
}

/// Every pane backend is a liveness source. Saves every backend impl
/// from repeating a one-line delegation, and keeps the reconciler
/// integration colocated with the trait whose contract it leans on.
impl<B: crate::backend::PaneBackend> LivenessSource for B {
    fn observe_panes(&self) -> PaneObservation {
        crate::backend::PaneBackend::observe_panes(self)
    }
    fn kind(&self) -> HostKind {
        crate::backend::PaneBackend::kind(self)
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
    /// One source per backend the daemon observes. Single-host daemons (and
    /// every test) carry exactly one; a multi-host daemon (tmux + herdr during
    /// a migration) carries several. Each tick observes all of them
    /// concurrently and reconciles each observation against the store under its
    /// own [`HostKind`], so a herdr timeout can't trigger tmux reaping or vice
    /// versa (`reconcile_observation` is completeness-gated per host).
    sources: Vec<Arc<L>>,
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
    /// If non-zero, fully orphaned rows (no pane, surface, or pid) idle for
    /// this long are flipped to `Stopped` on every tick so the GC can reap
    /// them. `Duration::ZERO` (default) disables the sweep. Targets paneless
    /// codex rows from detached `app-server`/remote sessions that no other
    /// converge path governs.
    paneless_stale_timeout: Duration,
    /// Root of codex's session-rollout tree (`~/.codex/sessions`). When
    /// `Some`, each tick reads every live codex row's rollout file and
    /// feeds its `rate_limits` through the store — the only way muxa learns
    /// a codex usage cap, since codex exposes no error/rate-limit hook.
    /// `None` (default) disables the poll.
    codex_sessions_root: Option<PathBuf>,
}

impl<L: LivenessSource> Reconciler<L> {
    pub fn new(store: SharedStore, source: L, interval: Duration) -> Self {
        Self::with_sources(store, vec![source], interval)
    }

    /// Build a reconciler that observes several backends per tick — the
    /// multi-host analog of [`Self::new`]. Every source is observed
    /// concurrently and reconciled under its own [`HostKind`]; the ghost
    /// age-out sweep ([`Store::mark_stale_cross_host_stopped`](crate::state::Store::mark_stale_cross_host_stopped))
    /// receives complete kinds plus intentionally-partial kinds. A complete
    /// source governs its rows directly; a partial source (such as cmux's
    /// current-surface-only adapter) protects hook-authoritative rows it cannot
    /// enumerate. A host outside the set, or a normally-authoritative source
    /// that cannot answer past the inactivity window, ages out. An empty
    /// `sources` degrades to a store-maintenance-only
    /// loop (no observation, but the stuck/paneless/codex sweeps still run);
    /// the daemon never constructs one that way — `active_backends()` is never
    /// empty.
    pub fn with_sources(store: SharedStore, sources: Vec<L>, interval: Duration) -> Self {
        Self {
            store,
            sources: sources.into_iter().map(Arc::new).collect(),
            interval,
            metrics: None,
            stuck_working_timeout: Duration::ZERO,
            stuck_waiting_timeout: Duration::ZERO,
            paneless_stale_timeout: Duration::ZERO,
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

    /// Enable age-based reaping of fully orphaned rows (no pane, surface, or
    /// pid) — flips them to `Stopped` after `t` of inactivity so the GC
    /// removes them. `Duration::ZERO` (the default) keeps the sweep off.
    #[must_use]
    pub fn with_paneless_stale_timeout(mut self, t: Duration) -> Self {
        self.paneless_stale_timeout = t;
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
                    tmux_socket: None,
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
    #[allow(clippy::too_many_lines)]
    pub async fn reconcile_once(&self) -> ReconcileReport {
        let started = Instant::now();
        // Every backend is observed each tick. Capture each observing host so
        // the store's reaping guard can exempt rows namespaced to a host that
        // is *also* in the set (cross-host migration) while reaping the ones
        // that aren't.
        let observing_kinds: Vec<HostKind> = self.sources.iter().map(|s| s.kind()).collect();
        // Pane observation shells out to tmux / round-trips the herdr socket and
        // must not block the runtime. Spawn every source's blocking observation
        // up front so they run CONCURRENTLY, then collect — a herdr timeout
        // must not serialize behind the tmux scan (and vice versa), keeping the
        // tick budget flat as backends are added.
        let list_started = Instant::now();
        let handles: Vec<(HostKind, tokio::task::JoinHandle<PaneObservation>)> = self
            .sources
            .iter()
            .map(|src| {
                let src = src.clone();
                let kind = src.kind();
                (
                    kind,
                    tokio::task::spawn_blocking(move || src.observe_panes()),
                )
            })
            .collect();
        let mut observations: Vec<(HostKind, PaneObservation)> = Vec::with_capacity(handles.len());
        for (kind, handle) in handles {
            let obs = handle
                .await
                .unwrap_or_else(|_| PaneObservation::incomplete(Vec::new()));
            observations.push((kind, obs));
        }
        let list_panes_us = u64::try_from(list_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        // The kinds whose observation was COMPLETE this tick — the hosts that
        // actually answered. Fixes 3/4/5 all key off this set rather than "every
        // observation is complete": one chronically-incomplete host must not
        // freeze the others.
        let all_complete = observations.iter().all(|(_, o)| o.is_complete());
        let complete_kinds: Vec<HostKind> = observations
            .iter()
            .filter(|(_, o)| o.is_complete())
            .map(|(k, _)| *k)
            .collect();
        // Complete scans govern their namespace directly. Intentionally
        // partial adapters cannot use absence as negative liveness evidence,
        // but their successful partial observation still proves the host is
        // structurally present and must protect hook rows from cross-host
        // stale aging. Transiently incomplete/failed scans do not join this
        // set and retain the existing 24h age-out behavior.
        let stale_protected_kinds: Vec<HostKind> = observations
            .iter()
            .filter(|(_, o)| o.protects_stale_rows())
            .map(|(k, _)| *k)
            .collect();
        let total_panes: usize = observations.iter().map(|(_, o)| o.panes.len()).sum();
        // Fix 3/5: the union of panes from COMPLETE observations only. An
        // incompletely-observed host contributes no panes, so its rows are
        // neither workload-reset nor used as codex-correlation candidates this
        // tick. Empty when no host answered (a single-host daemon whose one scan
        // failed) — then the workload scan+update is skipped entirely, keeping
        // that host's rows untouched (the old single-host-incomplete rule).
        let union_panes: Vec<crate::tmux::PaneInfo> = observations
            .iter()
            .filter(|(_, o)| o.is_complete())
            .flat_map(|(_, o)| o.panes.iter().cloned())
            .collect();
        // Workload scan runs once per tick over that union — process-tree
        // scanning is store-global (it clears the workload of any pane absent
        // from its map), so it must see every complete host's panes at once, and
        // `update_workloads` then governs only rows on a complete host (see
        // `Store::update_workloads`), leaving an incomplete host's rows as-is.
        let workload_started = Instant::now();
        let workloads = if complete_kinds.is_empty() {
            std::collections::HashMap::new()
        } else {
            let panes = union_panes.clone();
            tokio::task::spawn_blocking(move || process_tree::scan_pane_workloads(&panes))
                .await
                .unwrap_or_default()
        };
        let workload_scan_us =
            u64::try_from(workload_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let reconcile_started = Instant::now();
        let mut report = ReconcileReport::default();
        // Fix 5: correlate paneless codex ONCE over the union of complete panes,
        // BEFORE the per-host reap/dedup passes. Running it per host would only
        // show the ambiguity guard one host's panes, so a tmux pass could adopt a
        // row whose codex actually lives in a herdr pane at the same cwd. Doing
        // it here — ahead of the per-host passes — still lets this tick's dedup
        // demote the now-redundant synthetic. Skip when nothing answered.
        if !union_panes.is_empty() {
            report.paneless_correlated = self
                .store
                .correlate_paneless_codex_union(&union_panes)
                .await;
        }
        // Reconcile each observation against the store sequentially, under its
        // own host kind. Completeness is enforced per host inside
        // `reconcile_observation`, so an incomplete herdr scan is a no-op that
        // leaves tmux reaping untouched. Accumulate the per-host reports so the
        // timing line and callers (tests) see the whole tick's effect.
        for (kind, observation) in &observations {
            let r = self.store.reconcile_observation(observation, *kind).await;
            report.stale_panes_reaped += r.stale_panes_reaped;
            report.synthetic_demoted += r.synthetic_demoted;
            report.duplicates_collapsed += r.duplicates_collapsed;
        }
        let workload_changed = if complete_kinds.is_empty() {
            0
        } else {
            self.store
                .update_workloads(&workloads, &complete_kinds)
                .await
        };
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
        // Age out fully orphaned rows (no pane/surface/pid) so the GC can
        // reap them — otherwise paneless codex rows from detached
        // app-server/remote sessions accumulate without bound.
        let stale_paneless = self
            .store
            .mark_stale_paneless_stopped(self.paneless_stale_timeout)
            .await;
        if stale_paneless > 0 {
            tracing::info!(
                stale_paneless,
                "orphan-row sweep flipped {stale_paneless} paneless agent(s) to Stopped",
            );
        }
        // Age out rows whose pane belongs to a host that neither answered with
        // a complete observation nor intentionally exposes a partial namespace
        // this tick (e.g. a `zellij:` row while the set is tmux + herdr, a
        // `herdr:` row left behind after narrowing the set back to tmux, or a
        // normally-authoritative host that is chronically unable to answer).
        // The cross-host guard exempts foreign rows from *immediate* reaping,
        // and no observation reaps them, so without this they'd ghost forever.
        // Pass complete + intentionally-partial kinds, not every configured
        // kind. A failed authoritative host still ages out after the inactivity
        // window, while a structurally partial host such as cmux never turns an
        // unobserved-but-valid surface into a false Stopped row.
        let stale_cross_host = self
            .store
            .mark_stale_cross_host_stopped(&stale_protected_kinds, self.paneless_stale_timeout)
            .await;
        if stale_cross_host > 0 {
            tracing::info!(
                stale_cross_host,
                protected = ?stale_protected_kinds,
                "cross-host sweep flipped {stale_cross_host} foreign-host agent(s) to Stopped",
            );
        }
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
        let elapsed = started.elapsed();
        let elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        if elapsed >= Duration::from_secs(1) {
            warn!(
                elapsed_us,
                list_panes_us,
                workload_scan_us,
                store_update_us,
                panes = total_panes,
                pane_observation_complete = all_complete,
                backends = observing_kinds.len(),
                workloads = workloads.len(),
                stale = report.stale_panes_reaped,
                synthetic = report.synthetic_demoted,
                duplicates = report.duplicates_collapsed,
                correlated = report.paneless_correlated,
                workload_changed,
                "slow reconciler.tick",
            );
        } else {
            debug!(
                elapsed_us,
                list_panes_us,
                workload_scan_us,
                store_update_us,
                panes = total_panes,
                pane_observation_complete = all_complete,
                backends = observing_kinds.len(),
                workloads = workloads.len(),
                stale = report.stale_panes_reaped,
                synthetic = report.synthetic_demoted,
                duplicates = report.duplicates_collapsed,
                correlated = report.paneless_correlated,
                workload_changed,
                "reconciler.tick",
            );
        }
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
        observation: Mutex<PaneObservation>,
        kind: HostKind,
    }

    impl FakeLiveness {
        fn new(panes: Vec<PaneInfo>) -> Self {
            Self {
                observation: Mutex::new(PaneObservation::complete(panes)),
                kind: HostKind::Tmux,
            }
        }
        fn incomplete(panes: Vec<PaneInfo>) -> Self {
            Self {
                observation: Mutex::new(PaneObservation::incomplete(panes)),
                kind: HostKind::Tmux,
            }
        }
        fn partial(panes: Vec<PaneInfo>) -> Self {
            Self {
                observation: Mutex::new(PaneObservation::partial(panes)),
                kind: HostKind::Tmux,
            }
        }
        /// Tag this source with a specific observing host — used by the
        /// multi-source tests to exercise the cross-host reaping guard.
        fn with_kind(mut self, kind: HostKind) -> Self {
            self.kind = kind;
            self
        }
        fn set(&self, panes: Vec<PaneInfo>) {
            *self.observation.lock().unwrap() = PaneObservation::complete(panes);
        }
    }

    impl LivenessSource for FakeLiveness {
        fn observe_panes(&self) -> PaneObservation {
            self.observation.lock().unwrap().clone()
        }
        fn kind(&self) -> HostKind {
            self.kind
        }
    }

    /// Newtype that lets the loop test pass a shared `Arc<FakeLiveness>` to
    /// the runner while still mutating the live set from the outside via
    /// the original `Arc`. Owning a sole `FakeLiveness` would force us to
    /// snapshot live panes once at construction.
    struct ArcLiveness(Arc<FakeLiveness>);

    impl LivenessSource for ArcLiveness {
        fn observe_panes(&self) -> PaneObservation {
            self.0.observe_panes()
        }
    }

    fn pane(id: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_role: None,
            agent_alias: None,
            workspace_id: None,
            work_id: None,
            socket: None,
            pane_id: id.into(),
            session_id: String::new(),
            session: "s".into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    fn started(sid: &str, pane_id: &str, at: time::OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                surface: None,
                pane: Some(pane_id.into()),
                tmux_socket: None,
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

    fn herdr_pane(id: &str) -> PaneInfo {
        let mut p = pane(id);
        p.session = "w1".into();
        p
    }

    /// Multi-source reconcile: each backend governs only its own pane-id
    /// namespace. A tmux source reaps a dead tmux `%N` row and a herdr source
    /// reaps a dead `herdr:` row in the SAME tick, while each host's live row
    /// is preserved and neither host's observation touches the other's rows.
    #[tokio::test]
    async fn reconcile_once_reaps_per_host_across_a_backend_set() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&started("tmux-alive", "%1", t0)).await;
        store.apply(&started("tmux-ghost", "%2", t0)).await;
        store.apply(&started("herdr-alive", "herdr:p1", t0)).await;
        store.apply(&started("herdr-ghost", "herdr:p2", t0)).await;

        let tmux = FakeLiveness::new(vec![pane("%1")]).with_kind(HostKind::Tmux);
        let herdr = FakeLiveness::new(vec![herdr_pane("herdr:p1")]).with_kind(HostKind::Herdr);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, herdr], Duration::from_millis(10));
        let report = r.reconcile_once().await;

        // One stale reap per host, both in the one tick.
        assert_eq!(report.stale_panes_reaped, 2);
        let snap = store.snapshot().await;
        let live: std::collections::HashSet<&str> =
            snap.iter().map(|a| a.session_id.as_str()).collect();
        assert_eq!(live.len(), 2);
        assert!(live.contains("tmux-alive"));
        assert!(live.contains("herdr-alive"));
    }

    /// The cross-host age-out sweep spares rows whose host IS in the observed
    /// set (governed by that host's own reconcile pass) and ages out rows whose
    /// host is NOT — even when the set spans several hosts. A stale `zellij:`
    /// row is foreign to a tmux+herdr set and must flip to `Stopped`; a stale
    /// `herdr:` row is spared because herdr is observed (its own reconcile
    /// governs it — here its pane is still live, so it survives).
    #[tokio::test]
    async fn cross_host_sweep_uses_the_whole_observed_set() {
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&started("herdr-live", "herdr:p1", old)).await;
        store
            .apply(&started("zellij-foreign", "zellij:9", old))
            .await;

        let tmux = FakeLiveness::new(vec![pane("%1")]).with_kind(HostKind::Tmux);
        let herdr = FakeLiveness::new(vec![herdr_pane("herdr:p1")]).with_kind(HostKind::Herdr);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, herdr], Duration::from_millis(10))
                // Non-zero threshold enables the cross-host sweep; the rows are far
                // older than the cutoff so an unobserved host's row ages out now.
                .with_paneless_stale_timeout(Duration::from_secs(1));
        r.reconcile_once().await;

        let snap = store.snapshot().await;
        // herdr is observed (and its pane live) → spared.
        assert_eq!(
            snap.iter()
                .find(|a| a.session_id == "herdr-live")
                .map(|a| a.state),
            Some(AgentState::Idle),
        );
        // zellij is NOT observed → aged out to Stopped.
        assert_eq!(
            snap.iter()
                .find(|a| a.session_id == "zellij-foreign")
                .map(|a| a.state),
            Some(AgentState::Stopped),
        );
    }

    /// An incomplete observation from ONE backend must not reap or reset
    /// another backend's rows. The complete tmux scan governs its own rows; the
    /// incomplete herdr scan is a no-op, so `update_workloads` only touches
    /// tmux-namespaced rows and the herdr row keeps its metadata (see the
    /// dedicated store test `update_workloads_governs_only_complete_hosts`).
    #[tokio::test]
    async fn incomplete_one_backend_does_not_reap_or_reset_another() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&started("tmux-alive", "%1", t0)).await;
        store.apply(&started("herdr-alive", "herdr:p1", t0)).await;

        // tmux observed complete; herdr times out (incomplete, empty).
        let tmux = FakeLiveness::new(vec![pane("%1")]).with_kind(HostKind::Tmux);
        let herdr = FakeLiveness::incomplete(Vec::new()).with_kind(HostKind::Herdr);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, herdr], Duration::from_millis(10));
        let report = r.reconcile_once().await;

        // The incomplete herdr scan reaps nothing; the tmux scan reaps nothing
        // (its one row is live). Both rows survive.
        assert_eq!(report.stale_panes_reaped, 0);
        assert!(store.by_session("tmux-alive").await.is_some());
        assert!(store.by_session("herdr-alive").await.is_some());
    }

    /// Fix 4: the cross-host age-out keys off the COMPLETE-this-tick kinds, so a
    /// chronically-incomplete host's stale rows age out (nothing else ever
    /// reaps them) while a freshly-active row on the same host survives. tmux
    /// answers complete; herdr times out (incomplete) every tick, so `Herdr` is
    /// absent from `complete_kinds` and its rows are treated like a host outside
    /// the set. A herdr row idle past the window flips to `Stopped`; a herdr row
    /// with recent activity is spared by the last-activity threshold.
    #[tokio::test]
    async fn cross_host_ages_out_chronically_incomplete_host() {
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC);
        let fresh = time::OffsetDateTime::now_utc();
        store.apply(&started("herdr-stale", "herdr:p1", old)).await;
        store
            .apply(&started("herdr-fresh", "herdr:p2", fresh))
            .await;

        let tmux = FakeLiveness::new(vec![pane("%1")]).with_kind(HostKind::Tmux);
        // herdr never answers — incomplete, empty, every tick.
        let herdr = FakeLiveness::incomplete(Vec::new()).with_kind(HostKind::Herdr);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, herdr], Duration::from_millis(10))
                .with_paneless_stale_timeout(Duration::from_secs(1));
        r.reconcile_once().await;

        let snap = store.snapshot().await;
        assert_eq!(
            snap.iter()
                .find(|a| a.session_id == "herdr-stale")
                .map(|a| a.state),
            Some(AgentState::Stopped),
            "a chronically-incomplete host's stale row ages out",
        );
        assert_eq!(
            snap.iter()
                .find(|a| a.session_id == "herdr-fresh")
                .map(|a| a.state),
            Some(AgentState::Idle),
            "a freshly-active row on the same host survives",
        );
    }

    #[tokio::test]
    async fn intentionally_partial_host_protects_unobserved_hook_rows() {
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&started("cmux-unobserved", "cmux:surface-2", old))
            .await;

        let tmux = FakeLiveness::new(vec![pane("%1")]).with_kind(HostKind::Tmux);
        // cmux's first slice deliberately sees only the invoking surface. An
        // empty partial result is not evidence that another hooked surface
        // exited, even when its last event is older than the stale window.
        let cmux = FakeLiveness::partial(Vec::new()).with_kind(HostKind::Cmux);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, cmux], Duration::from_millis(10))
                .with_paneless_stale_timeout(Duration::from_secs(1));
        r.reconcile_once().await;

        assert_eq!(
            store
                .by_session("cmux-unobserved")
                .await
                .map(|agent| agent.state),
            Some(AgentState::Idle),
        );
    }

    fn codex_paneless(sid: &str, cwd: &str, at: time::OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::Codex,
                session_id: sid.into(),
                surface: None,
                pane: None,
                tmux_socket: None,
                cwd: Some(cwd.into()),
            },
            at,
        }
    }

    fn codex_synthetic(sid: &str, pane_id: &str, at: time::OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::Codex,
                session_id: sid.into(),
                surface: None,
                pane: Some(pane_id.into()),
                tmux_socket: None,
                cwd: None,
            },
            at,
        }
    }

    /// Fix 5: paneless-codex correlation runs ONCE over the union of complete
    /// observations, so its many-to-one cwd ambiguity guard sees candidate panes
    /// on *both* hosts. A paneless codex row whose cwd is shared by a tmux pane
    /// AND a herdr pane is ambiguous → NOT adopted (a per-host pass would have
    /// seen only one candidate and mis-adopted). A paneless row whose cwd is
    /// unique to a single host's pane still adopts as before.
    #[tokio::test]
    async fn codex_correlation_over_union_guards_cross_host_cwd_ambiguity() {
        let t0 = datetime!(2026-04-24 12:00:00 UTC);

        // Ambiguous: same cwd on a tmux pane and a herdr pane.
        let store = Store::shared();
        store
            .apply(&codex_synthetic("synthetic-%7", "%7", t0))
            .await;
        store
            .apply(&codex_synthetic("synthetic-herdr:p9", "herdr:p9", t0))
            .await;
        store.apply(&codex_paneless("real-amb", "/work", t0)).await;

        let mut tp = pane("%7");
        tp.current_path = "/work".into();
        let mut hp = herdr_pane("herdr:p9");
        hp.current_path = "/work".into();
        let tmux = FakeLiveness::new(vec![tp]).with_kind(HostKind::Tmux);
        let herdr = FakeLiveness::new(vec![hp]).with_kind(HostKind::Herdr);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, herdr], Duration::from_millis(10));
        let report = r.reconcile_once().await;

        assert_eq!(
            report.paneless_correlated, 0,
            "a cwd shared across hosts is ambiguous — no adoption",
        );
        assert!(
            store
                .by_session("real-amb")
                .await
                .is_some_and(|a| a.pane.is_none()),
            "the paneless row stays paneless",
        );

        // Unique: the cwd resolves to exactly one host's pane → adopt.
        let store = Store::shared();
        store
            .apply(&codex_synthetic("synthetic-%8", "%8", t0))
            .await;
        store.apply(&codex_paneless("real-solo", "/solo", t0)).await;

        let mut up = pane("%8");
        up.current_path = "/solo".into();
        let tmux = FakeLiveness::new(vec![up]).with_kind(HostKind::Tmux);
        let herdr = FakeLiveness::new(Vec::new()).with_kind(HostKind::Herdr);
        let r =
            Reconciler::with_sources(store.clone(), vec![tmux, herdr], Duration::from_millis(10));
        let report = r.reconcile_once().await;

        assert_eq!(report.paneless_correlated, 1, "unique cwd adopts as before");
        assert_eq!(
            store.by_session("real-solo").await.and_then(|a| a.pane),
            Some("%8".into()),
        );
    }

    #[tokio::test]
    async fn incomplete_full_failure_preserves_pane_rows_but_runs_safe_sweeps() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store.apply(&started("live", "%1", t0)).await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "live".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: None,
                    cwd: None,
                },
                prompt: "work".into(),
                at: t0,
            })
            .await;

        let source = FakeLiveness::incomplete(Vec::new());
        let r = Reconciler::new(store.clone(), source, Duration::from_millis(10))
            .with_stuck_working_timeout(Duration::from_secs(1));
        let report = r.reconcile_once().await;

        assert!(
            report.is_noop(),
            "failed observation must not remove the row"
        );
        let agent = store.by_session("live").await.expect("pane row preserved");
        assert_eq!(
            agent.state,
            AgentState::Idle,
            "pane-independent stuck-state maintenance should still run",
        );
    }

    #[tokio::test]
    async fn incomplete_partial_multi_socket_observation_is_not_authoritative() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:01:00 UTC);

        // Two historical rows on default exercise dedup; the amux row has
        // the same pane id but is absent from this partial observation and
        // therefore must not be reaped.
        store.apply(&started("default-old", "%1", t0)).await;
        store.apply(&started("default-new", "%1", t1)).await;
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "amux".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("/tmp/tmux-501/amux".into()),
                    cwd: None,
                },
                at: t1,
            })
            .await;

        // A synthetic codex pane plus a cwd-matching paneless real row would
        // normally be adopted and deduplicated by a complete pass.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "synthetic-7:default:%7".into(),
                    surface: None,
                    pane: Some("%7".into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "real-codex".into(),
                    surface: None,
                    pane: None,
                    tmux_socket: None,
                    cwd: Some("/work".into()),
                },
                at: t1,
            })
            .await;

        let mut default = pane("%1");
        default.socket = Some("default".into());
        let mut codex = pane("%7");
        codex.socket = Some("default".into());
        codex.current_path = "/work".into();
        let source = FakeLiveness::incomplete(vec![default, codex]);
        let r = Reconciler::new(store.clone(), source, Duration::from_millis(10));
        let report = r.reconcile_once().await;

        assert!(report.is_noop());
        assert_eq!(store.snapshot().await.len(), 5);
        assert!(store.by_session("amux").await.is_some());
        assert!(store.by_session("synthetic-7:default:%7").await.is_some());
        assert!(
            store
                .by_session("real-codex")
                .await
                .is_some_and(|agent| agent.pane.is_none()),
            "partial pane data must not drive cwd adoption",
        );
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
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::ClaudeCode,
            session_id: sid.into(),
            surface: None,
            pane: Some("%3".into()),
            cwd: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            state,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
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
                tmux_socket: None,
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
