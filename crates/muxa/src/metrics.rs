//! Lock-free runtime counters for the daemon.
//!
//! This module is the in-process counterpart to the `/api/metrics`
//! endpoint exposed by the dashboard: every counter is bumped at the
//! natural emission point (e.g. `Store::apply` bumps
//! `events_received_total`) and read out as a snapshot when an operator
//! curls the endpoint. Counters are intentionally lock-free
//! (`AtomicU64`) so the hot paths pay only a relaxed atomic add — no
//! contention, no allocations.
//!
//! Scope is deliberately small for v1: counters and "last elapsed"
//! gauges. We do not maintain histograms or quantiles in-process —
//! shipping accurate counters now is more useful than racing in a
//! hand-rolled HDR sketch for percentiles. If/when operators ask for
//! tail latencies we can layer that on without changing the wire shape.
//!
//! ## Lifetime
//!
//! A single [`Metrics`] is owned by [`Store`](crate::state::Store) and
//! cloned (via `Arc`) into any task that wants to bump a counter. The
//! `Store` exposes it via [`Store::metrics`](crate::state::Store::metrics)
//! so the dashboard's `AppState` can read it without a separate plumbing
//! channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Shared, lock-free runtime counters. Cloning is cheap (`Arc`-based).
///
/// All fields use [`Ordering::Relaxed`] — counters are observed
/// independently and we never rely on memory ordering between them.
/// Snapshots taken via [`Self::snapshot`] are point-in-time but not
/// cross-field-consistent; that's fine for an operator-facing JSON
/// dump where a 1-event skew between counters is invisible.
#[derive(Debug, Default)]
struct Inner {
    events_received_total: AtomicU64,
    snapshot_writes_total: AtomicU64,
    snapshot_last_write_elapsed_ms: AtomicU64,
    reconcile_passes_total: AtomicU64,
    sse_subscribers_current: AtomicU64,
    /// Wall-clock instant the daemon (more precisely, this `Metrics`)
    /// was constructed. Used to compute `uptime_secs` on snapshot.
    /// Not atomic because `Instant` is `Copy` and the field is set
    /// once at construction and never mutated.
    started_at: std::sync::OnceLock<Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

impl Metrics {
    /// Construct a fresh metrics handle. The "started at" instant is
    /// captured lazily on first read so tests can construct a `Metrics`
    /// without forcing an `Instant::now()` call into a `const` context.
    #[must_use]
    pub fn new() -> Self {
        let m = Self::default();
        // Set `started_at` eagerly. `OnceLock::set` is infallible here
        // because `m` is fresh.
        let _ = m.inner.started_at.set(Instant::now());
        m
    }

    /// Bump `events_received_total` by 1. Cheap relaxed atomic add.
    pub fn record_event(&self) {
        self.inner
            .events_received_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Bump `snapshot_writes_total` and stash the most recent elapsed
    /// time. Called by the snapshotter at the end of every successful
    /// write — failures are not counted, since the goal is "writes that
    /// hit disk", not "attempts".
    pub fn record_snapshot_write(&self, elapsed: Duration) {
        self.inner
            .snapshot_writes_total
            .fetch_add(1, Ordering::Relaxed);
        // Saturate to u64 max — a single write taking >584 million
        // years is implausible; even u32 overflow at >49 days is, but
        // u64 is what `as_millis` widens to and we lose nothing by
        // keeping that width.
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.inner
            .snapshot_last_write_elapsed_ms
            .store(ms, Ordering::Relaxed);
    }

    /// Bump `reconcile_passes_total` by 1. Called once per
    /// `Reconciler::reconcile_once` regardless of whether the pass
    /// produced any actual mutations — we want operators to see the
    /// loop is alive even when the registry is steady-state.
    pub fn record_reconcile_pass(&self) {
        self.inner
            .reconcile_passes_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the live SSE subscriber gauge. Paired with
    /// [`Self::sse_disconnect`] in the dashboard's stream handler.
    pub fn sse_connect(&self) -> u64 {
        // `fetch_add` returns the previous value; +1 gives the new one.
        self.inner
            .sse_subscribers_current
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    /// Decrement the live SSE subscriber gauge. Saturates at zero so a
    /// double-disconnect (shouldn't happen, but cheap to defend) can't
    /// underflow.
    pub fn sse_disconnect(&self) -> u64 {
        let prev = self
            .inner
            .sse_subscribers_current
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            // `fetch_update` only returns Err if the closure returns
            // None — we always return Some, so this is unreachable.
            .unwrap_or(0);
        prev.saturating_sub(1)
    }

    /// Read the current SSE subscriber count without mutating it.
    pub fn sse_subscribers(&self) -> u64 {
        self.inner.sse_subscribers_current.load(Ordering::Relaxed)
    }

    /// Take a point-in-time snapshot of every counter. Used by the
    /// `/api/metrics` handler.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let started_at = self.inner.started_at.get().copied();
        let uptime_secs = started_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or_default();
        MetricsSnapshot {
            uptime_secs,
            events_received_total: self.inner.events_received_total.load(Ordering::Relaxed),
            snapshot_writes_total: self.inner.snapshot_writes_total.load(Ordering::Relaxed),
            snapshot_last_write_elapsed_ms: self
                .inner
                .snapshot_last_write_elapsed_ms
                .load(Ordering::Relaxed),
            reconcile_passes_total: self.inner.reconcile_passes_total.load(Ordering::Relaxed),
            sse_subscribers_current: self.inner.sse_subscribers_current.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time view of every counter. Returned by
/// [`Metrics::snapshot`] and serialized by the `/api/metrics` handler
/// alongside per-state agent counts.
///
/// Field names are wire-stable: the dashboard's metrics endpoint
/// flattens this into a JSON object whose keys match these identifiers.
/// Renaming a field is a breaking change for any operator scraping the
/// endpoint.
#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub events_received_total: u64,
    pub snapshot_writes_total: u64,
    pub snapshot_last_write_elapsed_ms: u64,
    pub reconcile_passes_total: u64,
    pub sse_subscribers_current: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_event_increments_counter() {
        let m = Metrics::new();
        m.record_event();
        m.record_event();
        let snap = m.snapshot();
        assert_eq!(snap.events_received_total, 2);
    }

    #[test]
    fn record_snapshot_write_updates_count_and_elapsed() {
        let m = Metrics::new();
        m.record_snapshot_write(Duration::from_millis(7));
        m.record_snapshot_write(Duration::from_millis(11));
        let snap = m.snapshot();
        assert_eq!(snap.snapshot_writes_total, 2);
        // Last value wins.
        assert_eq!(snap.snapshot_last_write_elapsed_ms, 11);
    }

    #[test]
    fn sse_connect_disconnect_balance() {
        let m = Metrics::new();
        assert_eq!(m.sse_connect(), 1);
        assert_eq!(m.sse_connect(), 2);
        assert_eq!(m.sse_disconnect(), 1);
        assert_eq!(m.sse_subscribers(), 1);
        assert_eq!(m.sse_disconnect(), 0);
        // Saturating: one extra disconnect must not underflow.
        assert_eq!(m.sse_disconnect(), 0);
        assert_eq!(m.sse_subscribers(), 0);
    }

    #[test]
    fn reconcile_pass_increments_counter() {
        let m = Metrics::new();
        m.record_reconcile_pass();
        m.record_reconcile_pass();
        m.record_reconcile_pass();
        assert_eq!(m.snapshot().reconcile_passes_total, 3);
    }

    #[test]
    fn clones_share_state() {
        // Cloning the wrapper must share the underlying atomics so a
        // bump from one task is visible from another.
        let m = Metrics::new();
        let m2 = m.clone();
        m.record_event();
        m2.record_event();
        assert_eq!(m.snapshot().events_received_total, 2);
        assert_eq!(m2.snapshot().events_received_total, 2);
    }
}
